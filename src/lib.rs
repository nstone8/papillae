//reexport some stuff for our callers
pub use futures;
pub use iced;
pub use ralston;

use futures::stream::StreamExt;
use iced::futures::SinkExt;
use iced::widget::button::Button;
use iced::widget::image::{Handle, Image};
use iced::widget::text_input::TextInput;
use iced::widget::{text, Column, Container, Row};
use iced::{executor, subscription, Application, Command, Element, Subscription, Theme};
use ralston::image::DynamicImage;
use ralston::{Frame, FrameSource, FrameStream};
use std::fmt::Debug;
use std::future::pending;
use std::sync::mpsc::{channel, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

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

//going to make this a 'static type so we can pass it to threads
pub trait Analysis: 'static {
    ///Type that can be used to build a visualization of our progress
    type DisplayData: Default + Debug + Send + Clone;
    //Need a constructor which takes no arguments for our [iced] application
    //fn new() -> Self;
    ///Return a title for the window. This is an associated function, not a method
    fn get_title() -> String;
    ///process a frame coming off of the source. Argument is `&mut self` in case we want to store results within
    ///the type that implements `Analysis`. The sender is for optional display of a frame. This function should
    ///return data of type [Self::DisplayData] which can be used by [Self::display_results] to create
    ///customizable results for the user.
    fn process_frame(
        &mut self,
        frame: Frame,
        sender: futures::channel::mpsc::Sender<(DynamicImage, Self::DisplayData)>,
    );
    ///Build a results display for the user
    fn display_results(
        data: &Self::DisplayData,
    ) -> Container<'_, UiMessage<Self::DisplayData>, Theme, iced::Renderer>;
}

///Messages to send to running jobs
enum JobMessage<T> {
    Stop,
    ChangeConsumer(futures::channel::mpsc::Sender<(DynamicImage, T)>),
}

///Messages for driving our iced UI
#[derive(Debug, Clone)]
pub enum UiMessage<T> {
    Start,
    Stop,
    Preview,
    ChangeRes1(usize),
    ChangeRes2(usize),
    ChangeExposure(f64),
    UpdateFrame((Handle, T)),
    Pass, //in case we don't want to update
}

///`enum` to represent the fact that we could be running an [Analysis] or doing a preview
enum AnalysisOrPreview<A: Analysis> {
    Analysis(Arc<Mutex<A>>),
    Preview,
}

///Struct to represent a running analysis. I think we can keep this internal to papillae
struct AnalysisJob<T> {
    handle: JoinHandle<()>,
    controltx: Sender<JobMessage<T>>,
}

///Helper function which parses a string to a numeric type and prints an error
///message if it fails.
fn parse_str<T: std::str::FromStr>(s: &str) -> Option<T> {
    match str::parse::<T>(s) {
        Ok(num) => Some(num),
        Err(_) => {
            eprintln!("input value must be a number");
            None
        }
    }
}

impl<D: Default + Send + 'static + Clone> AnalysisJob<D> {
    ///build an `AnalysisJob` from an [Analysis]. `source_fn` should be a function or closure which
    ///returns a valid [FrameSource] when called with no arguments.
    fn new<A, T: FrameSource + 'static>(
        analysis: AnalysisOrPreview<A>,
        source_fn: fn() -> T,
        exposure: f64,
        resolution: [usize; 2],
    ) -> AnalysisJob<D>
    where
        A: Analysis<DisplayData = D> + std::marker::Send,
    {
        let (threadtx, threadrx) = channel::<JobMessage<D>>();
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
            let (sourcetx, sourcerx) = channel::<Frame>();
            //start the stream
            let stream = source.start(sourcetx);
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
                //we use the copy we made before we spawned the thread so we can attach the copy we received to the struct we're building
                match analysis {
                    //maybe we just want to borrow frametx? Going to leave it like this in case we want to pass it to other threads
                    AnalysisOrPreview::Analysis(ref a) => a.lock().unwrap().process_frame(
                        sourcerx.recv().expect("couldn't grab frame"),
                        frametx.clone(),
                    ),
                    AnalysisOrPreview::Preview => frametx
                        //we will send the default value of D for the result display
                        .try_send((
                            sourcerx.recv().expect("couldn't grab frame").to_image(),
                            Default::default(),
                        ))
                        .expect("couldn't send image"),
                }
            }
            stream.stop();
        });

        AnalysisJob::<D> {
            handle: thread_handle,
            controltx: threadtx,
        }
    }
    ///stop a running job
    fn stop(self) {
        self.controltx
            .send(JobMessage::Stop)
            .expect("Couldn't communicate to running job");
        self.handle.join().expect("job failed to stop");
    }
}

pub struct AnalysisInterface<F: FrameSource, A: Analysis> {
    analysis: Arc<Mutex<A>>,
    exposure: f64,
    resolution_1: usize,
    resolution_2: usize,
    dispframe: Handle,
    job: Option<AnalysisJob<A::DisplayData>>,
    source_fn: fn() -> F,
    display_data: A::DisplayData,
    process_buffer: usize,
}

pub struct InterfaceSettings<F: FrameSource, A: Analysis> {
    pub analysis: A,
    pub source_fn: fn() -> F,
    pub exposure: f64,
    pub resolution: [usize; 2],
    pub process_buffer: usize,
}
///UI for an Analysis
impl<F: FrameSource, A: Analysis> AnalysisInterface<F, A> {
    ///Create a new AnalysisInterface
    fn new(
        analysis: A,
        source_fn: fn() -> F,
        exposure: f64,
        resolution: [usize; 2],
        process_buffer: usize,
    ) -> AnalysisInterface<F, A> {
        let initial_pixels: Vec<u8> = vec![0, 0, 0, 0];
        let initial_handle = Handle::from_pixels(1, 1, initial_pixels);
        AnalysisInterface::<F, A> {
            analysis: Arc::new(Mutex::new(analysis)),
            exposure,
            resolution_1: resolution[0],
            resolution_2: resolution[1],
            dispframe: initial_handle,
            job: None,
            source_fn: source_fn,
            display_data: Default::default(),
            process_buffer,
        }
    }
    fn new_from_settings(i: InterfaceSettings<F, A>) -> AnalysisInterface<F, A> {
        AnalysisInterface::new(
            i.analysis,
            i.source_fn,
            i.exposure,
            i.resolution,
            i.process_buffer,
        )
    }
    ///Build an [iced] UI to display camera controls
    fn build_cam_ui<T>(&self) -> Column<'_, UiMessage<A::DisplayData>, T, iced::Renderer>
    where
        T: iced::widget::button::StyleSheet
            + iced::widget::text_input::StyleSheet
            + iced::widget::text::StyleSheet
            + 'static,
    {
        //first build our base camera-control center, with the camera view, capture settings
        //and start/preview buttons
        let im = Image::new(self.dispframe.clone());
        //we will make these mutable so we can enable them based on if we're running or not
        let mut exposure = TextInput::new("exposure time", &self.exposure.to_string());
        let mut resolution_1 = TextInput::new("h", &self.resolution_1.to_string());
        let mut resolution_2 = TextInput::new("w", &self.resolution_2.to_string());
        let mut preview_button: Button<UiMessage<A::DisplayData>, T, iced::Renderer> =
            Button::new("preview");
        let mut start_button = Button::new("start");
        let mut stop_button = Button::new("stop");
        if self.job.is_some() {
            //only the stop button should be enabled
            stop_button = stop_button.on_press(UiMessage::<A::DisplayData>::Stop);
        } else {
            //we should be allowed to edit everything or start
            resolution_1 = resolution_1.on_input(|s| match parse_str::<usize>(&s) {
                Some(num) => UiMessage::<A::DisplayData>::ChangeRes1(num),
                None => UiMessage::<A::DisplayData>::Pass,
            });
            resolution_2 = resolution_2.on_input(|s| match parse_str::<usize>(&s) {
                Some(num) => UiMessage::<A::DisplayData>::ChangeRes2(num),
                None => UiMessage::<A::DisplayData>::Pass,
            });
            exposure = exposure.on_input(|s| match parse_str::<f64>(&s) {
                Some(num) => UiMessage::<A::DisplayData>::ChangeExposure(num),
                None => UiMessage::<A::DisplayData>::Pass,
            });
            start_button = start_button.on_press(UiMessage::<A::DisplayData>::Start);
            preview_button = preview_button.on_press(UiMessage::<A::DisplayData>::Preview);
        }
        //group everyone into containers
        let res1_lab = Column::new().push(text("image height")).push(resolution_1);
        let res2_lab = Column::new().push(text("image width")).push(resolution_2);
        let exp_lab = Column::new().push(text("exposure")).push(exposure);
        let text_row = Row::new().push(exp_lab).push(res1_lab).push(res2_lab);
        let button_row = Row::new()
            .push(preview_button)
            .push(start_button)
            .push(stop_button);
        //return everyone all formatted
        Column::new().push(im).push(text_row).push(button_row)
    }
    /*
    fn get_analysis(&self) -> Arc<Mutex<A>> {
        Arc::clone(&self.analysis)
    }
    */
}

impl<F: FrameSource + 'static, A: Analysis + Send> Application for AnalysisInterface<F, A> {
    type Message = UiMessage<A::DisplayData>;
    type Executor = executor::Default;
    type Flags = InterfaceSettings<F, A>;
    type Theme = Theme;

    fn new(flags: Self::Flags) -> (AnalysisInterface<F, A>, Command<Self::Message>) {
        (
            AnalysisInterface::<F, A>::new_from_settings(flags),
            Command::none(),
        )
    }

    fn title(&self) -> String {
        A::get_title()
    }
    fn view(&self) -> Element<'_, Self::Message, Self::Theme, iced::Renderer> {
        Row::new()
            .push(Self::build_cam_ui::<Self::Theme>(&self))
            .push(<A as Analysis>::display_results(&self.display_data))
            .into()
    }
    //pick up here, deal with mutable analyses
    fn update(&mut self, m: Self::Message) -> Command<Self::Message> {
        match m {
            UiMessage::UpdateFrame((handle, display_data)) => {
                self.dispframe = handle;
                self.display_data = display_data;
            }
            //fill all of these out
            UiMessage::Start => {
                let job = AnalysisJob::new(
                    AnalysisOrPreview::Analysis(Arc::clone(&self.analysis)),
                    self.source_fn,
                    self.exposure,
                    [self.resolution_1, self.resolution_2],
                );
                let _ = self.job.insert(job);
            }
            UiMessage::Stop => {
                self.job
                    .take()
                    .expect("should only be able to stop when a job is running")
                    .stop();
            }
            UiMessage::ChangeRes1(r1) => self.resolution_1 = r1,
            UiMessage::ChangeRes2(r2) => self.resolution_2 = r2,
            UiMessage::ChangeExposure(e) => self.exposure = e,
            UiMessage::Pass => {}
            UiMessage::Preview => {
                let job = AnalysisJob::new(
                    AnalysisOrPreview::<A>::Preview,
                    self.source_fn,
                    self.exposure,
                    [self.resolution_1, self.resolution_2],
                );
                let _ = self.job.insert(job);
            }
        }
        Command::none()
    }
    fn subscription(&self) -> subscription::Subscription<Self::Message> {
        //we only need a subscription for frames if we're running
        match self.job {
            Some(ref j) => {
                //I think the point of this is to generate a unique id
                struct SomeWorker;
                //clone our sender so the worker can have a copy
                let threadtx = j.controltx.clone();
                //make a local copy of self.process_buffer that can be sent to the subscription
                let process_buffer = self.process_buffer.clone();
                subscription::channel(
                    std::any::TypeId::of::<SomeWorker>(),
                    100,
                    move |mut output| async move {
                        //register our existence with the frame grabber
                        let (frametx, mut framerx) = futures::channel::mpsc::channel::<(
                            DynamicImage,
                            A::DisplayData,
                        )>(process_buffer);
                        threadtx
                            .send(JobMessage::ChangeConsumer(frametx))
                            .expect("couldn't register with frame grabber");
                        loop {
                            if let Some((this_image, display_data)) = framerx.next().await {
                                let rgba = this_image.to_rgba8();
                                let handle = Handle::from_pixels(
                                    rgba.width(),
                                    rgba.height(),
                                    rgba.as_raw().clone(),
                                );
                                output
                                    .send(UiMessage::UpdateFrame::<A::DisplayData>((
                                        handle,
                                        display_data,
                                    )))
                                    .await
                                    .expect("couldn't send frame in subscription");
                            } else {
                                //wait for iced to kill this thread
                                let () = pending().await;
                            }
                        }
                    },
                )
            }
            None => Subscription::<UiMessage<A::DisplayData>>::none(),
        }
    }
}
