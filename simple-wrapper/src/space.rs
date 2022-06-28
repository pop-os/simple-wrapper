// SPDX-License-Identifier: MPL-2.0-only

use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    fs,
    os::unix::{net::UnixStream, prelude::AsRawFd},
    process::Child,
    rc::Rc,
    time::Instant,
};

use anyhow::bail;
use freedesktop_desktop_entry::{self, DesktopEntry, Iter};
use itertools::Itertools;
use libc::c_int;
use sctk::{
    environment::Environment,
    output::OutputInfo,
    reexports::{
        client::protocol::{wl_output as c_wl_output, wl_surface as c_wl_surface},
        client::{self, Attached, Main},
        protocols::{
            wlr::unstable::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1},
            xdg_shell::client::{
                xdg_popup,
                xdg_positioner::{Anchor, Gravity, XdgPositioner},
                xdg_surface::{self, XdgSurface},
                xdg_wm_base::XdgWmBase,
            },
        },
    },
    shm::AutoMemPool,
};
use simple_wrapper_config::SimpleWrapperConfig;
use slog::{info, trace, Logger};
use smithay::{
    backend::{
        egl::{
            context::{EGLContext, GlAttributes},
            display::EGLDisplay,
            ffi::{
                self,
                egl::{GetConfigAttrib, SwapInterval},
            },
            surface::EGLSurface,
        },
        renderer::{
            gles2::Gles2Renderer, utils::draw_surface_tree, Bind, Frame, ImportEgl, Renderer,
            Unbind,
        },
    },
    desktop::{
        draw_window,
        space::SurfaceTree,
        utils::{damage_from_surface_tree, send_frames_surface_tree},
        Kind, PopupKind, PopupManager, Space, Window, WindowSurfaceType,
    },
    nix::{fcntl, libc},
    reexports::wayland_server::{
        self, protocol::wl_surface::WlSurface as s_WlSurface, Client, Display as s_Display,
        DisplayHandle,
    },
    utils::{Logical, Rectangle, Size},
    wayland::{
        shell::xdg::{PopupSurface, PositionerState},
        SERIAL_COUNTER,
    },
};
use wayland_egl::WlEglSurface;
use xdg_shell_wrapper::{
    client_state::{Env, Focus},
    config::WrapperConfig,
    space::{ClientEglSurface, SpaceEvent, Visibility, WrapperSpace},
    util::{exec_child, get_client_sock},
};

impl Default for SimpleWrapperSpace {
    fn default() -> Self {
        Self {
            popup_manager: PopupManager::new(None),
            full_clear: false,
            config: Default::default(),
            log: Default::default(),
            space: Space::new(None),
            client: Default::default(),
            child: Default::default(),
            last_dirty: Default::default(),
            pending_dimensions: Default::default(),
            next_render_event: Default::default(),
            dimensions: Default::default(),
            focused_surface: Default::default(),
            pool: Default::default(),
            layer_shell: Default::default(),
            output: Default::default(),
            c_display: Default::default(),
            egl_display: Default::default(),
            renderer: Default::default(),
            layer_surface: Default::default(),
            egl_surface: Default::default(),
            layer_shell_wl_surface: Default::default(),
        }
    }
}

/// space for the cosmic panel
#[derive(Debug)]
pub struct SimpleWrapperSpace {
    /// config for the panel space
    pub config: SimpleWrapperConfig,
    /// logger for the panel space
    pub log: Option<Logger>,
    pub(crate) space: Space,
    pub(crate) popup_manager: PopupManager,
    pub(crate) client: Option<Client>,
    pub(crate) child: Option<Child>,
    pub(crate) last_dirty: Option<Instant>,
    pub(crate) pending_dimensions: Option<Size<i32, Logical>>,
    pub(crate) full_clear: bool,
    pub(crate) next_render_event: Rc<Cell<Option<SpaceEvent>>>,
    pub(crate) dimensions: Size<i32, Logical>,
    /// focused surface so it can be changed when a window is removed
    focused_surface: Rc<RefCell<Option<s_WlSurface>>>,
    /// visibility state of the panel / panel
    pub(crate) pool: Option<AutoMemPool>,
    pub(crate) layer_shell: Option<Attached<zwlr_layer_shell_v1::ZwlrLayerShellV1>>,
    pub(crate) output: Option<(c_wl_output::WlOutput, OutputInfo)>,
    pub(crate) c_display: Option<client::Display>,
    pub(crate) egl_display: Option<EGLDisplay>,
    pub(crate) renderer: Option<Gles2Renderer>,
    pub(crate) layer_surface: Option<Main<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>>,
    pub(crate) egl_surface: Option<Rc<EGLSurface>>,
    pub(crate) layer_shell_wl_surface: Option<Attached<c_wl_surface::WlSurface>>,
}

impl SimpleWrapperSpace {
    /// create a new space for the cosmic panel
    pub fn new(config: SimpleWrapperConfig, log: Logger) -> Self {
        Self {
            config,
            space: Space::new(log.clone()),
            popup_manager: PopupManager::new(log.clone()),
            log: Some(log),
            ..Default::default()
        }
    }

    fn constrain_dim(&self, size: Size<i32, Logical>) -> Size<i32, Logical> {
        let mut w = size.w.try_into().unwrap();
        let mut h = size.h.try_into().unwrap();
        w = 1.max(w);
        h = 1.max(h);
        if let (Some(w_range), _) = self.config.dimensions() {
            if w < w_range.start {
                w = w_range.start;
            } else if w > w_range.end {
                w = w_range.end;
            }
        }
        if let (_, Some(h_range)) = self.config.dimensions() {
            if h < h_range.start {
                h = h_range.start;
            } else if h > h_range.end {
                h = h_range.end;
            }
        }
        (w.try_into().unwrap(), h.try_into().unwrap()).into()
    }

    fn render(&mut self, dh: &DisplayHandle, time: u32) {
        if self.next_render_event.get() != None {
            return;
        }
        let clear_color = [0.0, 0.0, 0.0, 0.0];
        let renderer = self.renderer.as_mut().unwrap();
        renderer
            .bind(self.egl_surface.as_ref().unwrap().clone())
            .expect("Failed to bind surface to GL");

        let log_clone = self.log.clone().unwrap();
        let outputs = self.space.outputs().cloned().collect_vec();
        for o in outputs {
            for w in self.space.windows() {
                let mut damage = if self.full_clear {
                    vec![(w.bbox().to_physical(1))]
                } else {
                    let g = w.geometry();
    
                    // TODO handle scaling
                    w.accumulated_damage(g.loc.to_f64().to_physical(1.0), 1.0, Some((&self.space, &o)))
                };
    
                if damage.len() == 0 {
                    continue;
                }
                // dbg!(&damage);
                let _ = renderer.render(
                    self.dimensions.to_physical(1),
                    smithay::utils::Transform::Flipped180,
                    |renderer: &mut Gles2Renderer, frame| {
                        frame
                            .clear(clear_color, damage.iter().cloned().collect_vec().as_slice())
                            .expect("Failed to clear frame.");
                        let _ =
                            draw_window(dh, renderer, frame, w, 1.0, (0.0, 0.0), &damage, &log_clone);
                    },
                );
                self.egl_surface
                    .as_ref()
                    .unwrap()
                    .swap_buffers(Some(&mut damage))
                    .expect("Failed to swap buffers.");
            }    
        }
        let _ = renderer.unbind();
        self.space.send_frames(time);
        self.full_clear = false;
    }
}

impl WrapperSpace for SimpleWrapperSpace {
    type Config = SimpleWrapperConfig;

    fn handle_events(&mut self, dh: &DisplayHandle, time: u32, _: &Focus) -> Instant {
        if self
            .child
            .iter_mut()
            .map(|c| c.try_wait())
            .all(|r| matches!(r, Ok(Some(_))))
        {
            info!(
                self.log.as_ref().unwrap().clone(),
                "Child processes exited. Now exiting..."
            );
            std::process::exit(0);
        }
        let mut should_render = false;
        match self.next_render_event.take() {
            Some(SpaceEvent::Quit) => {
                trace!(
                    self.log.as_ref().unwrap(),
                    "root window removed, exiting..."
                );
                for child in &mut self.child {
                    let _ = child.kill();
                }
            }
            Some(SpaceEvent::Configure {
                width,
                height,
                serial: _serial,
            }) => {
                if self.dimensions != (width as i32, height as i32).into()
                    && self.pending_dimensions.is_none()
                {
                    self.dimensions = (width as i32, height as i32).into();
                    self.layer_shell_wl_surface.as_ref().unwrap().commit();
                    self.egl_surface
                        .as_ref()
                        .unwrap()
                        .resize(width as i32, height as i32, 0, 0);
                    self.full_clear = true;
                }
            }
            Some(SpaceEvent::WaitConfigure { width, height }) => {
                self.next_render_event
                    .replace(Some(SpaceEvent::WaitConfigure { width, height }));
            }
            None => {
                if let Some(d) = self.pending_dimensions.take() {
                    self.layer_surface
                        .as_ref()
                        .unwrap()
                        .set_size(d.w.try_into().unwrap(), d.h.try_into().unwrap());
                    self.layer_shell_wl_surface.as_ref().unwrap().commit();
                    self.next_render_event
                        .replace(Some(SpaceEvent::WaitConfigure {
                            width: d.w,
                            height: d.h,
                        }));
                } else {
                    should_render = true;
                }
            }
        }

        if should_render {
            self.render(dh, time);
        }
        if self.egl_surface.as_ref().unwrap().get_size() != Some(self.dimensions.to_physical(1)) {
            self.full_clear = true;
        }

        self.last_dirty.unwrap_or_else(|| Instant::now())
    }

    fn handle_button(&mut self, c_focused_surface: &c_wl_surface::WlSurface) {
        if self.focused_surface.borrow().is_none()
            && **self.layer_shell_wl_surface.as_ref().unwrap() == *c_focused_surface
        {
            self.close_popups()
        }
    }

    fn add_top_level(&mut self, w: Window) {
        self.full_clear = true;

        let wl_surface = w.toplevel().wl_surface().clone();
        self.focused_surface.borrow_mut().replace(wl_surface.clone());
        self.space.map_window(&w, (0, 0), true);
        self.space.commit(&wl_surface);
    }

    fn add_popup(
        &mut self,
        _: &Environment<Env>,
        _: &Attached<XdgWmBase>,
        s_surface: PopupSurface,
        _: Main<XdgPositioner>,
        _: PositionerState,
    ) {
        self.close_popups();
        let wl_surface = s_surface.wl_surface().clone();
        let _ = self.popup_manager.track_popup(PopupKind::Xdg(s_surface));
        self.popup_manager.commit(&wl_surface);
        // let parent_g = if let Some(s) = self.space.windows().find(|s| Some(s.toplevel().wl_surface()) == s_surface.get_parent_surface().as_ref()) {
        //     s.geometry()
        // } else {
        //     return;
        // };

        // positioner.set_size(rect_size.w, rect_size.h);
        // positioner.set_anchor_rect(
        //     anchor_rect.loc.x + parent_g.loc.x,
        //     anchor_rect.loc.y + parent_g.loc.y,
        //     anchor_rect.size.w,
        //     anchor_rect.size.h,
        // );
        // positioner.set_anchor(Anchor::from_raw(anchor_edges as u32).unwrap_or(Anchor::None));
        // positioner.set_gravity(Gravity::from_raw(gravity as u32).unwrap_or(Gravity::None));

        // positioner.set_constraint_adjustment(u32::from(constraint_adjustment));
        // positioner.set_offset(offset.x, offset.y);
        // if positioner.as_ref().version() >= 3 {
        //     if reactive {
        //         positioner.set_reactive();
        //     }
        //     if let Some(parent_size) = parent_size {
        //         positioner.set_parent_size(parent_size.w, parent_size.h);
        //     }
        // }
    }

    fn close_popups(&mut self) {
        for w in &mut self.space.windows() {
            for (PopupKind::Xdg(p), _) in
                PopupManager::popups_for_surface(w.toplevel().wl_surface())
            {
                p.send_popup_done();
                self.popup_manager.commit(p.wl_surface());
            }
        }
    }

    ///  update active window based on pointer location
    fn update_pointer(&mut self, (x, y): (i32, i32)) {
        let point = (x, y);
        // set new focused
        if let Some((_, s, _)) = self
            .space
            .surface_under((x as f64, y as f64), WindowSurfaceType::ALL)
        {
            self.focused_surface.borrow_mut().replace(s);
            return;
        }
        self.focused_surface.borrow_mut().take();
    }

    fn reposition_popup(
        &mut self,
        s_popup: PopupSurface,
        _: Main<XdgPositioner>,
        _: PositionerState,
        token: u32,
    ) -> anyhow::Result<()> {
        s_popup.send_repositioned(token);
        s_popup.send_configure()?;
        self.popup_manager.commit(s_popup.wl_surface());

        Ok(())
    }

    fn next_space_event(&self) -> Rc<Cell<Option<SpaceEvent>>> {
        Rc::clone(&self.next_render_event)
    }

    fn config(&self) -> Self::Config {
        self.config.clone()
    }

    fn spawn_clients(
        &mut self,
        display: &mut DisplayHandle,
    ) -> Result<Vec<UnixStream>, anyhow::Error> {
        if self.child.is_none() {
            let (client, sockets) = get_client_sock(display);
            self.client = Some(client);
            // TODO how slow is this? Would it be worth using a faster method of comparing strings?
            self.child = Some(
                Iter::new(freedesktop_desktop_entry::default_paths())
                    .find_map(|path| {
                        if Some(OsString::from(self.config.applet()).as_os_str())
                            == path.file_stem()
                        {
                            let raw_fd = sockets.as_raw_fd();
                            let fd_flags = fcntl::FdFlag::from_bits(
                                fcntl::fcntl(raw_fd, fcntl::FcntlArg::F_GETFD).unwrap(),
                            )
                            .unwrap();
                            fcntl::fcntl(
                                raw_fd,
                                fcntl::FcntlArg::F_SETFD(
                                    fd_flags.difference(fcntl::FdFlag::FD_CLOEXEC),
                                ),
                            )
                            .unwrap();
                            fs::read_to_string(&path).ok().and_then(|bytes| {
                                if let Ok(entry) = DesktopEntry::decode(&path, &bytes) {
                                    if let Some(exec) = entry.exec() {
                                        let requests_host_wayland_display =
                                            entry.desktop_entry("HostWaylandDisplay").is_some();
                                        return Some(exec_child(
                                            exec,
                                            Some(self.config.name()),
                                            self.log.as_ref().unwrap().clone(),
                                            raw_fd,
                                            requests_host_wayland_display,
                                        ));
                                    }
                                }
                                None
                            })
                        } else {
                            None
                        }
                    })
                    .expect("Failed to spawn client..."),
            );
            Ok(vec![sockets])
        } else {
            bail!("Clients have already been spawned!");
        }
    }

    fn add_output(
        &mut self,
        output: Option<&c_wl_output::WlOutput>,
        output_info: Option<&OutputInfo>,
        pool: AutoMemPool,
        c_display: client::Display,
        layer_shell: Attached<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
        log: Logger,
        c_surface: Attached<c_wl_surface::WlSurface>,
        focused_surface: Rc<RefCell<Option<s_WlSurface>>>,
    ) -> anyhow::Result<()> {
        if self.layer_shell_wl_surface.is_some()
            || self.output.is_some()
            || self.layer_shell.is_some()
        {
            bail!("output already added!")
        }

        let dimensions = self.constrain_dim((1, 1).into());

        let layer_surface =
            layer_shell.get_layer_surface(&c_surface, output, self.config.layer(), "".to_owned());

        layer_surface.set_anchor(self.config.anchor.into());
        layer_surface.set_keyboard_interactivity(self.config.keyboard_interactivity());
        layer_surface.set_size(dimensions.w.try_into().unwrap(), dimensions.h.try_into().unwrap());

        // Commit so that the server will send a configure event
        c_surface.commit();

        let next_render_event = Rc::new(Cell::new(Some(SpaceEvent::WaitConfigure {
            width: dimensions.w,
            height: dimensions.h,
        })));

        //let egl_surface_clone = egl_surface.clone();
        let next_render_event_handle = next_render_event.clone();
        let logger = log.clone();
        layer_surface.quick_assign(move |layer_surface, event, _| {
            match (event, next_render_event_handle.get()) {
                (zwlr_layer_surface_v1::Event::Closed, _) => {
                    info!(logger, "Received close event. closing.");
                    next_render_event_handle.set(Some(SpaceEvent::Quit));
                }
                (
                    zwlr_layer_surface_v1::Event::Configure {
                        serial,
                        width,
                        height,
                    },
                    next,
                ) if next != Some(SpaceEvent::Quit) => {
                    trace!(
                        logger,
                        "received configure event {:?} {:?} {:?}",
                        serial,
                        width,
                        height
                    );
                    layer_surface.ack_configure(serial);
                    next_render_event_handle.set(Some(SpaceEvent::Configure {
                        width: width.try_into().unwrap(),
                        height: height.try_into().unwrap(),
                        serial: serial.try_into().unwrap(),
                    }));
                }
                (_, _) => {}
            }
        });

        let client_egl_surface = ClientEglSurface {
            wl_egl_surface: WlEglSurface::new(&c_surface, dimensions.w, dimensions.h),
            display: c_display.clone(),
        };
        let egl_display = EGLDisplay::new(&client_egl_surface, log.clone())
            .expect("Failed to initialize EGL display");

        let egl_context = EGLContext::new_with_config(
            &egl_display,
            GlAttributes {
                version: (3, 0),
                profile: None,
                debug: cfg!(debug_assertions),
                vsync: false,
            },
            Default::default(),
            log.clone(),
        )
        .expect("Failed to initialize EGL context");

        let mut min_interval_attr = 23239;
        unsafe {
            GetConfigAttrib(
                egl_display.get_display_handle().handle,
                egl_context.config_id(),
                ffi::egl::MIN_SWAP_INTERVAL as c_int,
                &mut min_interval_attr,
            );
        }

        let renderer = unsafe {
            Gles2Renderer::new(egl_context, log.clone()).expect("Failed to initialize EGL Surface")
        };
        trace!(log, "{:?}", unsafe {
            SwapInterval(egl_display.get_display_handle().handle, 0)
        });

        let egl_surface = Rc::new(
            EGLSurface::new(
                &egl_display,
                renderer
                    .egl_context()
                    .pixel_format()
                    .expect("Failed to get pixel format from EGL context "),
                renderer.egl_context().config_id(),
                client_egl_surface,
                log.clone(),
            )
            .expect("Failed to initialize EGL Surface"),
        );

        let next_render_event_handle = next_render_event.clone();
        let logger = log.clone();
        layer_surface.quick_assign(move |layer_surface, event, _| {
            match (event, next_render_event_handle.get()) {
                (zwlr_layer_surface_v1::Event::Closed, _) => {
                    info!(logger, "Received close event. closing.");
                    next_render_event_handle.set(Some(SpaceEvent::Quit));
                }
                (
                    zwlr_layer_surface_v1::Event::Configure {
                        serial,
                        width,
                        height,
                    },
                    next,
                ) if next != Some(SpaceEvent::Quit) => {
                    trace!(
                        logger,
                        "received configure event {:?} {:?} {:?}",
                        serial,
                        width,
                        height
                    );
                    layer_surface.ack_configure(serial);
                    next_render_event_handle.set(Some(SpaceEvent::Configure {
                        width: width.try_into().unwrap(),
                        height: height.try_into().unwrap(),
                        serial: serial.try_into().unwrap(),
                    }));
                }
                (_, _) => {}
            }
        });

        self.output = output.cloned().zip(output_info.cloned());
        self.egl_display.replace(egl_display);
        self.renderer.replace(renderer);
        self.layer_shell.replace(layer_shell);
        self.c_display.replace(c_display);
        self.pool.replace(pool);
        self.layer_surface.replace(layer_surface);
        self.egl_surface.replace(egl_surface);
        self.dimensions = dimensions;
        self.focused_surface = focused_surface;
        self.next_render_event = next_render_event;
        self.full_clear = true;
        self.layer_shell_wl_surface = Some(c_surface);

        Ok(())
    }

    fn log(&self) -> Option<Logger> {
        self.log.clone()
    }

    fn destroy(&mut self) {
        self.layer_surface.as_mut().map(|ls| ls.destroy());
        self.layer_shell_wl_surface
            .as_mut()
            .map(|wls| wls.destroy());
    }

    fn space(&mut self) -> &mut Space {
        &mut self.space
    }

    fn popup_manager(&mut self) -> &mut PopupManager {
        &mut self.popup_manager
    }

    fn visibility(&self) -> Visibility {
        Visibility::Visible
    }

    fn raise_window(&mut self, w: &Window, activate: bool) {
        self.space.raise_window(w, activate);
    }

    fn dirty_window(&mut self, s: &s_WlSurface) {
        if let Some(w) = self.space.window_for_surface(s, WindowSurfaceType::ALL) {
            let size = self.constrain_dim(w.geometry().size);
            if self.dimensions != size {
                if let Some((_, _)) = &self.output {
                    // TODO improve this for when there are changes to the lists of plugins while running
                    let size = self.constrain_dim(size);
                    let pending_dimensions = self.pending_dimensions.unwrap_or(self.dimensions);
                    let mut wait_configure_dim = self
                        .next_render_event
                        .get()
                        .map(|e| match e {
                            SpaceEvent::Configure {
                                width,
                                height,
                                serial: _serial,
                            } => (width, height),
                            SpaceEvent::WaitConfigure { width, height } => (width, height),
                            _ => self.dimensions.into(),
                        })
                        .unwrap_or(pending_dimensions.into());
                    if self.dimensions.w < size.w
                        && pending_dimensions.w < size.w
                        && wait_configure_dim.0 < size.w
                    {
                        self.pending_dimensions =
                            Some((size.w, wait_configure_dim.1).into());
                        wait_configure_dim.0 = size.w;
                    }
                    if self.dimensions.h < size.h
                        && pending_dimensions.h < size.h
                        && wait_configure_dim.1 < size.h
                    {
                        self.pending_dimensions =
                            Some((wait_configure_dim.0, size.h).into());
                    }
                } else {
                    if self
                        .next_render_event
                        .get()
                        .map(|e| match e {
                            SpaceEvent::Configure {
                                width,
                                height,
                                serial: _serial,
                            } => (width, height).into(),
                            SpaceEvent::WaitConfigure { width, height } => (width, height).into(),
                            _ => self.dimensions,
                        })
                        .unwrap_or(self.pending_dimensions.unwrap_or(self.dimensions))
                        != size
                    {
                        self.pending_dimensions = Some(size);
                        self.full_clear = true;
                    }
                }
            }
            self.space.commit(s);
        }
    }

    fn dirty_popup(&mut self, s: &s_WlSurface) {}
}

// impl Drop for SimpleWrapperSpace {
//     fn drop(&mut self) {
//         self.destroy();
//     }
// }
