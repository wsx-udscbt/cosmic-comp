use std::{
    collections::HashMap,
    fmt,
    hash::{Hash, Hasher},
    sync::{mpsc::Receiver, Arc, Mutex},
};

pub use cosmic::Renderer as IcedRenderer;
use cosmic::Theme;
use cosmic::{
    iced_native::{
        command::Action,
        event::Event,
        keyboard::{Event as KeyboardEvent, Modifiers as IcedModifiers},
        mouse::{Button as MouseButton, Event as MouseEvent, ScrollDelta},
        program::{Program as IcedProgram, State},
        renderer::Style,
        window::{Event as WindowEvent, Id},
        Command, Debug, Point as IcedPoint, Size as IcedSize,
    },
    Element,
};
use iced_softbuffer::{
    native::{raqote::DrawTarget, *},
    Backend,
};

use ordered_float::OrderedFloat;
use smithay::{
    backend::{
        allocator::Fourcc,
        input::{ButtonState, KeyState},
        renderer::{
            element::{
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
                AsRenderElements,
            },
            ImportMem, Renderer,
        },
    },
    desktop::space::{RenderZindex, SpaceElement},
    input::{
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerTarget, RelativeMotionEvent},
        Seat,
    },
    output::Output,
    reexports::calloop::RegistrationToken,
    reexports::calloop::{self, futures::Scheduler, LoopHandle},
    utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Serial, Size, Transform},
};

#[derive(Debug)]
pub struct IcedElement<P: Program + Send + 'static>(Arc<Mutex<IcedElementInternal<P>>>);

// SAFETY: We cannot really be sure about `iced_native::program::State` sadly,
// but the rest should be fine.
unsafe impl<P: Program + Send + 'static> Send for IcedElementInternal<P> {}

impl<P: Program + Send + 'static> Clone for IcedElement<P> {
    fn clone(&self) -> Self {
        IcedElement(self.0.clone())
    }
}

impl<P: Program + Send + 'static> PartialEq for IcedElement<P> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl<P: Program + Send + 'static> Eq for IcedElement<P> {}

impl<P: Program + Send + 'static> Hash for IcedElement<P> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0) as usize).hash(state)
    }
}

pub trait Program {
    type Message: std::fmt::Debug + Send;
    fn update(
        &mut self,
        message: Self::Message,
        loop_handle: &LoopHandle<'static, crate::state::Data>,
    ) -> Command<Self::Message> {
        let _ = (message, loop_handle);
        Command::none()
    }
    fn view(&self) -> Element<'_, Self::Message>;

    fn background(&self, target: &mut DrawTarget<&mut [u32]>) {
        let _ = target;
    }
    fn foreground(&self, target: &mut DrawTarget<&mut [u32]>) {
        let _ = target;
    }
}

struct ProgramWrapper<P: Program>(P, LoopHandle<'static, crate::state::Data>);
impl<P: Program> IcedProgram for ProgramWrapper<P> {
    type Message = <P as Program>::Message;
    type Renderer = IcedRenderer;

    fn update(&mut self, message: Self::Message) -> Command<Self::Message> {
        self.0.update(message, &self.1)
    }

    fn view(&self) -> Element<'_, Self::Message> {
        self.0.view()
    }
}

struct IcedElementInternal<P: Program + Send + 'static> {
    // draw buffer
    outputs: Vec<Output>,
    buffers: HashMap<OrderedFloat<f64>, (MemoryRenderBuffer, bool)>,

    // state
    size: Size<i32, Logical>,
    cursor_pos: Option<Point<f64, Logical>>,

    // iced
    theme: Theme,
    renderer: IcedRenderer,
    state: State<ProgramWrapper<P>>,
    debug: Debug,

    // futures
    handle: LoopHandle<'static, crate::state::Data>,
    scheduler: Scheduler<<P as Program>::Message>,
    executor_token: Option<RegistrationToken>,
    rx: Receiver<<P as Program>::Message>,
}

impl<P: Program + Send + 'static> fmt::Debug for IcedElementInternal<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IcedElementInternal")
            .field("buffers", &"...")
            .field("size", &self.size)
            .field("cursor_pos", &self.cursor_pos)
            .field("theme", &self.theme)
            .field("renderer", &"...")
            .field("state", &"...")
            .field("debug", &self.debug)
            .field("handle", &self.handle)
            .field("scheduler", &self.scheduler)
            .field("executor_token", &self.executor_token)
            .field("rx", &self.rx)
            .finish()
    }
}

impl<P: Program + Send + 'static> Drop for IcedElementInternal<P> {
    fn drop(&mut self) {
        self.handle.remove(self.executor_token.take().unwrap());
    }
}

impl<P: Program + Send + 'static> IcedElement<P> {
    pub fn new(
        program: P,
        size: impl Into<Size<i32, Logical>>,
        handle: LoopHandle<'static, crate::state::Data>,
    ) -> IcedElement<P> {
        let size = size.into();
        let mut renderer = IcedRenderer::new(Backend::new());
        let mut debug = Debug::new();

        let state = State::new(
            ProgramWrapper(program, handle.clone()),
            IcedSize::new(size.w as f32, size.h as f32),
            &mut renderer,
            &mut debug,
        );

        let (executor, scheduler) = calloop::futures::executor().expect("Out of file descriptors");
        let (tx, rx) = std::sync::mpsc::channel();
        let executor_token = handle
            .insert_source(executor, move |message, _, _| {
                let _ = tx.send(message);
            })
            .ok();

        let mut internal = IcedElementInternal {
            outputs: Vec::new(),
            buffers: HashMap::new(),
            size,
            cursor_pos: None,
            theme: Theme::dark(), // TODO
            renderer,
            state,
            debug,
            handle,
            scheduler,
            executor_token,
            rx,
        };
        let _ = internal.update(true);

        IcedElement(Arc::new(Mutex::new(internal)))
    }

    pub fn with_program<R>(&self, func: impl FnOnce(&P) -> R) -> R {
        let internal = self.0.lock().unwrap();
        func(&internal.state.program().0)
    }

    pub fn loop_handle(&self) -> LoopHandle<'static, crate::state::Data> {
        self.0.lock().unwrap().handle.clone()
    }

    pub fn resize(&self, size: Size<i32, Logical>) {
        let mut internal = self.0.lock().unwrap();
        let internal_ref = &mut *internal;
        if internal_ref.size == size {
            return;
        }

        internal_ref.size = size;
        for (scale, (buffer, needs_redraw)) in internal_ref.buffers.iter_mut() {
            let buffer_size = internal_ref
                .size
                .to_f64()
                .to_buffer(**scale, Transform::Normal)
                .to_i32_round();
            *buffer =
                MemoryRenderBuffer::new(Fourcc::Argb8888, buffer_size, 1, Transform::Normal, None);
            *needs_redraw = true;
        }
        internal_ref.update(true);
    }

    pub fn force_update(&self) {
        let mut internal = self.0.lock().unwrap();
        for (_buffer, ref mut needs_redraw) in internal.buffers.values_mut() {
            *needs_redraw = true;
        }
        internal.update(true);
    }
}

impl<P: Program + Send + 'static> IcedElementInternal<P> {
    fn update(&mut self, mut force: bool) -> Vec<Action<<P as Program>::Message>> {
        while let Ok(message) = self.rx.try_recv() {
            self.state.queue_message(message);
            force = true;
        }

        if !force {
            return Vec::new();
        }

        let cursor_pos = self.cursor_pos.unwrap_or(Point::from((-1.0, -1.0)));

        let actions = self
            .state
            .update(
                IcedSize::new(self.size.w as f32, self.size.h as f32),
                IcedPoint::new(cursor_pos.x as f32, cursor_pos.y as f32),
                &mut self.renderer,
                &self.theme,
                &Style {
                    text_color: self.theme.cosmic().on_bg_color().into(),
                },
                &mut cosmic::iced_native::clipboard::Null,
                &mut self.debug,
            )
            .1
            .map(|command| command.actions());

        if actions.is_some() {
            for (_buffer, ref mut needs_redraw) in self.buffers.values_mut() {
                *needs_redraw = true;
            }
        }
        let actions = actions.unwrap_or_default();
        actions
            .into_iter()
            .filter_map(|action| {
                if let Action::Future(future) = action {
                    let _ = self.scheduler.schedule(future);
                    None
                } else {
                    Some(action)
                }
            })
            .collect::<Vec<_>>()
    }
}

impl<P: Program + Send + 'static> PointerTarget<crate::state::State> for IcedElement<P> {
    fn enter(
        &self,
        _seat: &Seat<crate::state::State>,
        _data: &mut crate::state::State,
        event: &MotionEvent,
    ) {
        let mut internal = self.0.lock().unwrap();
        internal
            .state
            .queue_event(Event::Mouse(MouseEvent::CursorEntered));
        let position = IcedPoint::new(event.location.x as f32, event.location.y as f32);
        internal
            .state
            .queue_event(Event::Mouse(MouseEvent::CursorMoved { position }));
        internal.cursor_pos = Some(event.location);
        let _ = internal.update(true);
    }

    fn motion(
        &self,
        _seat: &Seat<crate::state::State>,
        _data: &mut crate::state::State,
        event: &MotionEvent,
    ) {
        let mut internal = self.0.lock().unwrap();
        let position = IcedPoint::new(event.location.x as f32, event.location.y as f32);
        internal
            .state
            .queue_event(Event::Mouse(MouseEvent::CursorMoved { position }));
        internal.cursor_pos = Some(event.location);
        let _ = internal.update(true);
    }

    fn relative_motion(
        &self,
        _seat: &Seat<crate::state::State>,
        _data: &mut crate::state::State,
        _event: &RelativeMotionEvent,
    ) {
    }

    fn button(
        &self,
        _seat: &Seat<crate::state::State>,
        _data: &mut crate::state::State,
        event: &ButtonEvent,
    ) {
        let mut internal = self.0.lock().unwrap();
        let button = match event.button {
            0x110 => MouseButton::Left,
            0x111 => MouseButton::Right,
            0x112 => MouseButton::Middle,
            x => MouseButton::Other(x as u8),
        };
        internal.state.queue_event(Event::Mouse(match event.state {
            ButtonState::Pressed => MouseEvent::ButtonPressed(button),
            ButtonState::Released => MouseEvent::ButtonReleased(button),
        }));
        let _ = internal.update(true);
    }

    fn axis(
        &self,
        _seat: &Seat<crate::state::State>,
        _data: &mut crate::state::State,
        frame: AxisFrame,
    ) {
        let mut internal = self.0.lock().unwrap();
        internal
            .state
            .queue_event(Event::Mouse(MouseEvent::WheelScrolled {
                delta: if let Some(discrete) = frame.discrete {
                    ScrollDelta::Lines {
                        x: discrete.0 as f32,
                        y: discrete.1 as f32,
                    }
                } else {
                    ScrollDelta::Pixels {
                        x: frame.axis.0 as f32,
                        y: frame.axis.1 as f32,
                    }
                },
            }));
        let _ = internal.update(true);
    }

    fn leave(
        &self,
        _seat: &Seat<crate::state::State>,
        _data: &mut crate::state::State,
        _serial: Serial,
        _time: u32,
    ) {
        let mut internal = self.0.lock().unwrap();
        internal
            .state
            .queue_event(Event::Mouse(MouseEvent::CursorLeft));
        let _ = internal.update(true);
    }
}

impl<P: Program + Send + 'static> KeyboardTarget<crate::state::State> for IcedElement<P> {
    fn enter(
        &self,
        _seat: &Seat<crate::state::State>,
        _data: &mut crate::state::State,
        _keys: Vec<KeysymHandle<'_>>,
        _serial: Serial,
    ) {
        // TODO convert keys
    }

    fn leave(
        &self,
        _seat: &Seat<crate::state::State>,
        _data: &mut crate::state::State,
        _serial: Serial,
    ) {
        // TODO remove all held keys
    }

    fn key(
        &self,
        _seat: &Seat<crate::state::State>,
        _data: &mut crate::state::State,
        _key: KeysymHandle<'_>,
        _state: KeyState,
        _serial: Serial,
        _time: u32,
    ) {
        // TODO convert keys
    }

    fn modifiers(
        &self,
        _seat: &Seat<crate::state::State>,
        _data: &mut crate::state::State,
        modifiers: ModifiersState,
        _serial: Serial,
    ) {
        let mut internal = self.0.lock().unwrap();
        let mut mods = IcedModifiers::empty();
        if modifiers.shift {
            mods.insert(IcedModifiers::SHIFT);
        }
        if modifiers.alt {
            mods.insert(IcedModifiers::ALT);
        }
        if modifiers.ctrl {
            mods.insert(IcedModifiers::CTRL);
        }
        if modifiers.logo {
            mods.insert(IcedModifiers::LOGO);
        }
        internal
            .state
            .queue_event(Event::Keyboard(KeyboardEvent::ModifiersChanged(mods)));
        let _ = internal.update(true);
    }
}

impl<P: Program + Send + 'static> IsAlive for IcedElement<P> {
    fn alive(&self) -> bool {
        true
    }
}

impl<P: Program + Send + 'static> SpaceElement for IcedElement<P> {
    fn bbox(&self) -> Rectangle<i32, Logical> {
        Rectangle::from_loc_and_size((0, 0), self.0.lock().unwrap().size)
    }

    fn is_in_input_region(&self, _point: &Point<f64, Logical>) -> bool {
        true
    }

    fn set_activate(&self, activated: bool) {
        let mut internal = self.0.lock().unwrap();
        internal.state.queue_event(Event::Window(
            Id::MAIN,
            if activated {
                WindowEvent::Focused
            } else {
                WindowEvent::Unfocused
            },
        ));
        let _ = internal.update(true); // TODO
    }

    fn output_enter(&self, output: &Output, _overlap: Rectangle<i32, Logical>) {
        let mut internal = self.0.lock().unwrap();
        let scale = output.current_scale().fractional_scale();
        if !internal.buffers.contains_key(&OrderedFloat(scale)) {
            let buffer_size = internal
                .size
                .to_f64()
                .to_buffer(scale, Transform::Normal)
                .to_i32_round();
            internal.buffers.insert(
                OrderedFloat(scale),
                (
                    MemoryRenderBuffer::new(
                        Fourcc::Argb8888,
                        buffer_size,
                        1,
                        Transform::Normal,
                        None,
                    ),
                    true,
                ),
            );
        }
        internal.outputs.push(output.clone());
    }

    fn output_leave(&self, output: &Output) {
        self.0.lock().unwrap().outputs.retain(|o| o != output);
        self.refresh();
    }

    fn z_index(&self) -> u8 {
        // meh, user-provided?
        RenderZindex::Shell as u8
    }

    fn refresh(&self) {
        let mut internal = self.0.lock().unwrap();
        // makes partial borrows easier
        let internal_ref = &mut *internal;
        internal_ref.buffers.retain(|scale, _| {
            internal_ref
                .outputs
                .iter()
                .any(|o| o.current_scale().fractional_scale() == **scale)
        });
        for scale in internal_ref
            .outputs
            .iter()
            .map(|o| OrderedFloat(o.current_scale().fractional_scale()))
            .filter(|scale| !internal_ref.buffers.contains_key(scale))
            .collect::<Vec<_>>()
            .into_iter()
        {
            let buffer_size = internal_ref
                .size
                .to_f64()
                .to_buffer(*scale, Transform::Normal)
                .to_i32_round();
            internal_ref.buffers.insert(
                scale,
                (
                    MemoryRenderBuffer::new(
                        Fourcc::Argb8888,
                        buffer_size,
                        1,
                        Transform::Normal,
                        None,
                    ),
                    true,
                ),
            );
        }
    }
}

impl<P, R> AsRenderElements<R> for IcedElement<P>
where
    P: Program + Send + 'static,
    R: Renderer + ImportMem,
    <R as Renderer>::TextureId: 'static,
{
    type RenderElement = MemoryRenderBufferRenderElement<R>;

    fn render_elements<C: From<Self::RenderElement>>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        let mut internal = self.0.lock().unwrap();

        let _ = internal.update(false); // TODO

        // makes partial borrows easier
        let internal_ref = &mut *internal;
        if let Some((buffer, ref mut needs_redraw)) =
            internal_ref.buffers.get_mut(&OrderedFloat(scale.x))
        {
            let size = internal_ref
                .size
                .to_f64()
                .to_buffer(scale.x, Transform::Normal)
                .to_i32_round();

            if *needs_redraw && size.w > 0 && size.h > 0 {
                let renderer = &mut internal_ref.renderer;
                let state_ref = &internal_ref.state;
                buffer
                    .render()
                    .draw(move |buf| {
                        let mut target = raqote::DrawTarget::from_backing(
                            size.w,
                            size.h,
                            bytemuck::cast_slice_mut::<_, u32>(buf),
                        );

                        target.clear(raqote::SolidSource::from_unpremultiplied_argb(0, 0, 0, 0));
                        state_ref.program().0.background(&mut target);

                        let draw_options = raqote::DrawOptions {
                            // Default to antialiasing off for now
                            antialias: raqote::AntialiasMode::None,
                            ..Default::default()
                        };

                        // Having at least one clip fixes some font rendering issues
                        target.push_clip_rect(raqote::IntRect::new(
                            raqote::IntPoint::new(0, 0),
                            raqote::IntPoint::new(size.w, size.h),
                        ));

                        renderer.with_primitives(|backend, primitives| {
                            for primitive in primitives.iter() {
                                draw_primitive(
                                    &mut target,
                                    &draw_options,
                                    backend,
                                    scale.x as f32,
                                    primitive,
                                );
                            }
                        });

                        state_ref.program().0.foreground(&mut target);
                        Result::<_, ()>::Ok(vec![Rectangle::from_loc_and_size((0, 0), size)])
                    })
                    .unwrap();
                *needs_redraw = false;
            }

            if let Ok(buffer) = MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                location.to_f64(),
                &buffer,
                Some(alpha),
                Some(Rectangle::from_loc_and_size(
                    (0., 0.),
                    size.to_f64().to_logical(1.0, Transform::Normal),
                )),
                Some(internal_ref.size),
            ) {
                return vec![C::from(buffer)];
            }
        }
        Vec::new()
    }
}
