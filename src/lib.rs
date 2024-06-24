use std::sync::mpsc::{channel, Sender, TryRecvError};
use std::sync::{Arc,Mutex};
use std::thread::{self,JoinHandle};
use std::ops::Deref;
use futures;
use futures::stream::StreamExt;
use iced::futures::SinkExt;
use iced::widget::image::{Handle, Image};
use iced::widget::{Container,Column,Row,text};
use iced::widget::container::StyleSheet;
use iced::widget::button::Button;
use iced::widget::text_input::TextInput;
use iced::advanced::Renderer;
use iced::{executor, subscription, Subscription, Application, Command, Element, Theme};
use image::buffer::ConvertBuffer;
use image::{ImageBuffer, Rgba, Luma, RgbaImage, Pixel};
use ralston::{FrameSource,Frame};
use palette::convert::IntoColor;

/*
///A trait representing some type of real time image analysis
pub trait Analysis {
	/// type of data communicated to the user while running
	type RunData;
	///Start a job operating on a [FrameSource] with [CaptureSettings].
	///`framesender` should be where the running job should place images which
	///should be displayed to the user as the job runs
	fn start<F:FrameSource>(source:&F,settings:CaptureSettings,
		updatesender:Sender<(Option<Frame<F::PixelType,F::ImageContainerType>>,Option<Self::RunData>)>) -> AnalysisJob;
	///update our current view based on new `RunData`
	fn update(&mut self,rd:Self::RunData);
	///Generate an update view for the user
	fn view<'a,Message,Theme:StyleSheet,R:Renderer>(&self) -> Container<'a,Message,Theme,R>;
}
*/

pub trait Analysis {
	///Need a constructor which takes no arguments for our [iced] application
	fn new() -> Self;
	///Return a title for the window
	fn get_title(&self) -> String;
	///process a frame coming off of the source. Argument is `&mut self` in case we want to store results within
	///the type that implements `Analysis`. The sender is for optional display of the frame
	fn process_frame<P:Pixel,T: Deref<Target = [P::Subpixel]>>(&mut self, frame: Frame<P,T>, sender: futures::channel::mpsc::Sender<ImageBuffer<P,T>>);
	///Display our results to the user as we run
	fn display_results<'a,Message,Theme:StyleSheet,R:Renderer>(&self) -> Container<'a,Message,Theme,R>;
}

///Messages to send to running jobs
pub enum JobMessage<P:Pixel,C: Deref<Target = [P::Subpixel]>> {
	Stop,
	ChangeConsumer(futures::channel::mpsc::Sender<ImageBuffer<P,C>>)
}

///Messages for driving our iced UI
#[derive(Debug, Clone)]
pub enum UiMessage {
	Start,
	Stop,
	Preview,
	ChangeRes1(usize),
	ChangeRes2(usize),
	ChangeExposure(f64),
	UpdateFrame(Handle),
	Pass //in case we don't want to update
}

///`enum` to represent the fact that we could be running an [Analysis] or doing a preview
enum AnalysisOrPreview<A:Analysis> {
	Analysis(Arc<Mutex<A>>),
	Preview
}

///Struct to represent a running analysis. I think we can keep this internal to papillae
struct AnalysisJob<F:FrameSource,A:Analysis> {
	analysis: AnalysisOrPreview<A>,
	handle: JoinHandle<()>,
	controltx: Sender<JobMessage<F::PixelType,F::ImageContainerType>>,
}

///Helper function which parses a string to a numeric type and prints an error
///message if it fails. 
fn parse_str<T: std::str::FromStr>(s:&str) -> Option<T> {
	match str::parse::<T>(s) {
		Ok(num) => Some(num),
		Err(_) => {
			eprintln!("input value must be a number");
			None
		}
	}
}

impl<T:FrameSource,A:Analysis> AnalysisJob<T,A> {
	///build an `AnalysisJob` from an [Analysis]. `source_fn` should be a function or closure which
	///returns a valid [FrameSource] when called with no arguments.
	fn new<F>(analysis:AnalysisOrPreview<A>,source_fn:F,exposure:f64,resolution: [usize;2]) -> AnalysisJob<T,A> where T:FrameSource, F: Fn() -> T + Send, A:Analysis + std::marker::Send,  <T as FrameSource>::ImageContainerType: std::marker::Send, <T as FrameSource>::PixelType: std::marker::Send{
		let (threadtx, threadrx) = channel::<JobMessage<T::PixelType,T::ImageContainerType>>();
		let thread_handle = thread::spawn(move || {
            let mut source = source_fn();
            //change the exposure
            source.set_exposure(exposure);
            source.set_resolution(resolution);
            //wait until we have a consumer of frames to start the stream
            let Ok(JobMessage::ChangeConsumer(mut frametx)) = threadrx.recv() else {
                panic!("couldn't get a consumer for frames");
            };
            //make a channel for our source
            let (sourcetx, sourcerx) = channel::<Frame<T::PixelType,T::ImageContainerType>>();
            //start the stream
            let _stream = source.start(sourcetx);
            //shove frames until asked to stop
            loop {
                //check to make sure we're using the right channel and that we should keep going
                match threadrx.try_recv() {
                    //new consumer
                    Ok(JobMessage::ChangeConsumer(newframetx)) => frametx = newframetx,
                    //time to die
                    Err(TryRecvError::Disconnected) => break,
                    Ok(JobMessage::Stop) => break,
                    //keep going
                    Err(TryRecvError::Empty) => {}
                }
                //grab a frame and process it.
				match analysis {
					//maybe we just want to borrow frametx? Going to leave it like this in case we want to pass it to other threads
					AnalysisOrPreview::Analysis(a) => a.lock().unwrap().process_frame(sourcerx.recv().expect("couldn't grab frame"),frametx.clone()),
					AnalysisOrPreview::Preview => frametx.try_send(sourcerx.recv().expect("couldn't grab frame").to_image()).expect("couldn't send image"),
				}
            }
        });
        
		AnalysisJob::<T,A> {
			analysis,
			handle: thread_handle,
			controltx: threadtx,
		}
	}
	///stop a running job
	fn stop(self) {
		self.controltx.send(JobMessage::Stop).expect("Couldn't communicate to running job");
		self.handle.join().expect("job failed to stop");
	}
}

struct AnalysisInterface<F:FrameSource,A:Analysis,V:Fn() -> F> where <F as FrameSource>::ImageContainerType: Sync + Send, <F as FrameSource>::PixelType: Sync + Send{
	analysis: Arc<Mutex<A>>,
	exposure: f64,
	resolution_1:usize,
	resolution_2:usize,
	dispframe: Handle,
	job: Option<AnalysisJob<F,A>>,
	source_fn: Box<V>
}

struct InterfaceSettings<F:FrameSource,A:Analysis,V:Fn() -> F> where <F as FrameSource>::ImageContainerType: Sync + Send, <F as FrameSource>::PixelType: Sync + Send{
	analysis:A,
	source_fn:V,
	exposure:f64,
	resolution:[usize;2]
}
///UI for an Analysis
impl<F:FrameSource,A:Analysis,V:Fn() -> F> AnalysisInterface<F,A,V> where <F as FrameSource>::ImageContainerType: Sync + Send, <F as FrameSource>::PixelType: Sync + Send{
	///Create a new AnalysisInterface
	fn new(analysis:A,source_fn:V,exposure:f64,resolution:[usize;2]) -> AnalysisInterface<F,A,V>{
		let initial_pixels:Vec<u8> = vec![0, 0, 0, 0];
        let initial_handle = Handle::from_pixels(1, 1, initial_pixels);
		AnalysisInterface::<F,A,V>{
			analysis: Arc::new(Mutex::new(analysis)),
			exposure,
			resolution_1: resolution[1],
			resolution_2: resolution[2],
			dispframe: initial_handle,
			job: None,
			source_fn: Box::new(source_fn)
		}
	}
	fn new_from_settings(i:InterfaceSettings<F,A,V>) -> AnalysisInterface<F,A,V> {
		AnalysisInterface::new(i.analysis,i.source_fn,i.exposure,i.resolution)
	}
	///Build an [iced] UI to display camera controls
	fn build_cam_ui(&self) -> Column<'_,UiMessage,_,iced::Renderer> {
		//first build our base camera-control center, with the camera view, capture settings
		//and start/preview buttons
		let im = Image::new(self.dispframe.clone());
		//we will make these mutable so we can enable them based on if we're running or not
		let mut exposure = TextInput::new("exposure time",&self.exposure.to_string());
		let mut resolution_1 = TextInput::new("h",&self.resolution_1.to_string());
		let mut resolution_2 = TextInput::new("w",&self.resolution_2.to_string());
		let mut preview_button = Button::new("preview");
		let mut start_button = Button::new("start");
		let mut stop_button = Button::new("stop");
		if self.job.is_some() {
			//only the stop button should be enabled
			stop_button = stop_button.on_press(UiMessage::Stop);
		} else {
			//we should be allowed to edit everything or start
			resolution_1 = resolution_1.on_input(|s| {
				match parse_str::<usize>(&s){
					Some(num) => UiMessage::ChangeRes1(num),
					None => UiMessage::Pass
				}					
			});
			resolution_2 = resolution_2.on_input(|s| {
				match parse_str::<usize>(&s){
					Some(num) => UiMessage::ChangeRes2(num),
					None => UiMessage::Pass
				}					
			});
			exposure = exposure.on_input(|s| {
				match parse_str::<f64>(&s){
					Some(num) => UiMessage::ChangeExposure(num),
					None => UiMessage::Pass
				}					
			});
			start_button = start_button.on_press(UiMessage::Start);
		}
		//group everyone into containers
		let res1_lab = Column::new().push(text("image height")).push(resolution_1);
		let res2_lab = Column::new().push(text("image width")).push(resolution_2);
		let exp_lab = Column::new().push(text("exposure")).push(exposure);
		let text_row = Row::new().push(exp_lab).push(res1_lab).push(res2_lab);
		let button_row = Row::new().push(start_button).push(stop_button);
		//return everyone all formatted
		Column::new().push(im).push(text_row).push(button_row)
	}
	fn get_analysis(&self) -> Arc<Mutex<A>> {
		self.analysis
	}
}

impl<F:FrameSource, A:Analysis + Send, V:Fn() -> F + Send> Application for AnalysisInterface<F,A,V> where (F::PixelType):IntoColor<Rgba<u8>>, <F as FrameSource>::ImageContainerType: Sync + Send, <F as FrameSource>::PixelType: Sync + Send{
	type Message = UiMessage;
    type Executor = executor::Default;
    type Flags = InterfaceSettings<F,A,V>;
    type Theme = Theme;

    fn new(flags: Self::Flags) -> (AnalysisInterface<F,A,V>, Command<Self::Message>) {
        (AnalysisInterface::<F,A,V>::new_from_settings(flags),Command::none())
    }

    fn title(&self) -> String {
        self.get_analysis().lock().unwrap().get_title()
    }
    fn view(&self) -> Element<'_, Self::Message, Self::Theme, iced::Renderer> {
		let analysis_results = self.get_analysis().lock().unwrap().display_results::<'_,Self::Message,Self::Theme,iced::Renderer>();
		Row::new().push(self::build_cam_ui::<Self::Theme, iced::Renderer>(&self)).push(analysis_results).into()
    }
	//pick up here, deal with mutable analyses
    fn update(&mut self, m: Self::Message) -> Command<Self::Message> {
        match m {
            UiMessage::UpdateFrame(handle) => {
                self.dispframe = handle;
            },
			//fill all of these out
			UiMessage::Start => {
				let job = AnalysisJob::<F,A>::new(AnalysisOrPreview::Analysis(Arc::clone(&self.analysis)),self.source_fn,self.exposure,[self.resolution_1,self.resolution_2]);
				self.job.insert(job);
			},
			UiMessage::Stop => {
				self.job.take().expect("should only be able to stop when a job is running").stop();
			},
			UiMessage::ChangeRes1(r1) => self.resolution_1 = r1,
			UiMessage::ChangeRes2(r2) => self.resolution_2 = r2,
			UiMessage::ChangeExposure(e) => self.exposure = e,
			UiMessage::Pass => {},
			UiMessage::Preview => {
				let job = AnalysisJob::<F,A>::new(AnalysisOrPreview::Preview,self.source_fn,self.exposure,[self.resolution_1,self.resolution_2]);
				self.job.insert(job);
			}				
        }
        Command::none()
    }
    fn subscription(&self) -> subscription::Subscription<Self::Message> {
		
		//we only need a subscription for frames if we're running
		match self.job {
		Some(j) => {
        //I think the point of this is to generate a unique id
        struct SomeWorker;
        //clone our sender so the worker can have a copy
        let threadtx = j.controltx.clone();
        subscription::channel(
            std::any::TypeId::of::<SomeWorker>(),
            100,
            |mut output| async move {
                //register our existence with the frame grabber
                let (frametx, mut framerx) = futures::channel::mpsc::channel::<ImageBuffer<F::PixelType,F::ImageContainerType>>(30);
                threadtx
                    .send(JobMessage::ChangeConsumer(frametx))
                    .expect("couldn't register with frame grabber");
                loop {
					let this_image = framerx.select_next_some().await;
					let rgba: RgbaImage = this_image.convert();
					let handle = Handle::from_pixels(rgba.width(), rgba.height(), rgba.as_raw().clone());
                    output
                        .send(UiMessage::UpdateFrame(handle))
                        .await
                        .expect("couldn't send frame in subscription");
                }
            },
        )
		},
		None => Subscription::<UiMessage>::none()//fill me out
	}
	}
	
}