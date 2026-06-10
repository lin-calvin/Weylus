use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, trace, warn};

use dbus::{
    arg::{PropMap, Variant},
    blocking::SyncConnection,
    message::{MatchRule, MessageType},
};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSink;

use crate::capturable::{Capturable, Geometry, Recorder};
use crate::video::PixelProvider;

#[derive(Debug)]
pub struct DBusError(String);

impl std::fmt::Display for DBusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self(s) = self;
        write!(f, "{}", s)
    }
}

impl Error for DBusError {}

#[derive(Debug)]
pub struct GStreamerError(String);

impl std::fmt::Display for GStreamerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self(s) = self;
        write!(f, "{}", s)
    }
}

impl Error for GStreamerError {}

struct MutterSession {
    conn: Arc<SyncConnection>,
    session_path: dbus::Path<'static>,
}

impl MutterSession {
    fn stop(&self) {
        let proxy = self.conn.with_proxy(
            "org.gnome.Mutter.ScreenCast",
            &self.session_path,
            Duration::from_millis(1000),
        );
        let res: Result<(), dbus::Error> =
            proxy.method_call("org.gnome.Mutter.ScreenCast.Session", "Stop", ());
        if let Err(err) = res {
            warn!("Failed to stop Mutter ScreenCast session: {}", err);
        }
    }
}

#[derive(Clone)]
pub struct MutterCapturable {
    session: Arc<MutterSession>,
    node_id: u32,
    width: u32,
    height: u32,
}

impl Capturable for MutterCapturable {
    fn name(&self) -> String {
        format!("Virtual Display {}x{}", self.width, self.height)
    }

    fn geometry(&self) -> Result<Geometry, Box<dyn Error>> {
        Ok(Geometry::Relative(0.0, 0.0, 1.0, 1.0))
    }

    fn before_input(&mut self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }

    fn recorder(&self, _capture_cursor: bool) -> Result<Box<dyn Recorder>, Box<dyn Error>> {
        Ok(Box::new(MutterRecorder::new(self.clone())?))
    }
}

pub struct MutterRecorder {
    session: Arc<MutterSession>,
    buffer: Option<gst::MappedBuffer<gst::buffer::Readable>>,
    buffer_cropped: Vec<u8>,
    pix_fmt: String,
    is_cropped: bool,
    pipeline: gst::Pipeline,
    appsink: AppSink,
    width: usize,
    height: usize,
}

impl MutterRecorder {
    pub fn new(capturable: MutterCapturable) -> Result<Self, Box<dyn Error>> {
        let pipeline = gst::Pipeline::new();

        let src = gst::ElementFactory::make("pipewiresrc").build()?;
        src.set_property("path", &format!("{}", capturable.node_id));
        src.set_property("always-copy", &true);

        let sink = gst::ElementFactory::make("appsink").build()?;
        sink.set_property("drop", &true);
        sink.set_property("max-buffers", &1u32);

        pipeline.add_many(&[&src, &sink])?;
        src.link(&sink)?;
        let appsink = sink
            .dynamic_cast::<AppSink>()
            .map_err(|_| GStreamerError("Sink element is expected to be an appsink!".into()))?;
        let mut caps = gst::Caps::new_empty();
        caps.merge_structure(gst::structure::Structure::from_iter(
            "video/x-raw",
            [("format", "BGRx".into())],
        ));
        caps.merge_structure(gst::structure::Structure::from_iter(
            "video/x-raw",
            [("format", "RGBx".into())],
        ));
        appsink.set_caps(Some(&caps));

        pipeline.set_state(gst::State::Playing)?;
        Ok(Self {
            session: capturable.session,
            pipeline,
            appsink,
            buffer: None,
            pix_fmt: "".into(),
            width: 0,
            height: 0,
            buffer_cropped: vec![],
            is_cropped: false,
        })
    }
}

impl Recorder for MutterRecorder {
    fn capture(&mut self) -> Result<PixelProvider<'_>, Box<dyn Error>> {
        if let Some(sample) = self
            .appsink
            .try_pull_sample(gst::ClockTime::from_mseconds(16))
        {
            let cap = sample.caps().unwrap().structure(0).unwrap();
            let w: i32 = cap.value("width")?.get()?;
            let h: i32 = cap.value("height")?.get()?;
            self.pix_fmt = cap.value("format")?.get()?;
            let w = w as usize;
            let h = h as usize;
            let buf = sample
                .buffer_owned()
                .ok_or_else(|| GStreamerError("Failed to get owned buffer.".into()))?;
            let mut crop = buf
                .meta::<gstreamer_video::VideoCropMeta>()
                .map(|m| m.rect());
            if Some((0, 0, w as u32, h as u32)) == crop {
                crop = None;
            }
            let buf = buf
                .into_mapped_buffer_readable()
                .map_err(|_| GStreamerError("Failed to map buffer.".into()))?;
            let buf_size = buf.size();
            if buf_size != (w * h * 4) {
                trace!(
                    "Size of mapped buffer: {} does NOT match size of capturable {}x{}@BGRx, \
                    dropping it!",
                    buf_size,
                    w,
                    h
                );
            } else {
                if let Some((x_off, y_off, w_crop, h_crop)) = crop {
                    let x_off = x_off as usize;
                    let y_off = y_off as usize;
                    let w_crop = w_crop as usize;
                    let h_crop = h_crop as usize;
                    self.buffer_cropped.clear();
                    let data = buf.as_slice();
                    self.buffer_cropped.reserve(w_crop * h_crop * 4);
                    for y in y_off..(y_off + h_crop) {
                        let i = 4 * (w * y + x_off);
                        self.buffer_cropped.extend(&data[i..i + 4 * w_crop]);
                    }
                    self.width = w_crop;
                    self.height = h_crop;
                } else {
                    self.width = w;
                    self.height = h;
                }
                self.is_cropped = crop.is_some();
                self.buffer = Some(buf);
            }
        } else {
            trace!("No new buffer available, falling back to previous one.");
        }
        if self.buffer.is_none() {
            return Err(Box::new(GStreamerError("No buffer available!".into())));
        }
        let buf = if self.is_cropped {
            self.buffer_cropped.as_slice()
        } else {
            self.buffer.as_ref().unwrap().as_slice()
        };
        match self.pix_fmt.as_str() {
            "BGRx" => Ok(PixelProvider::BGR0(self.width, self.height, buf)),
            "RGBx" => Ok(PixelProvider::RGB0(self.width, self.height, buf)),
            _ => unreachable!(),
        }
    }
}

impl Drop for MutterRecorder {
    fn drop(&mut self) {
        if let Err(err) = self.pipeline.set_state(gst::State::Null) {
            warn!("Failed to stop GStreamer pipeline: {}.", err);
        }
        self.session.stop();
    }
}

pub fn get_virtual_display_capturable(
    width: u32,
    height: u32,
    capture_cursor: bool,
) -> Result<MutterCapturable, Box<dyn Error>> {
    let conn = SyncConnection::new_session()?;

    let proxy = conn.with_proxy(
        "org.gnome.Mutter.ScreenCast",
        "/org/gnome/Mutter/ScreenCast",
        Duration::from_millis(1000),
    );

    let args: PropMap = HashMap::new();
    let session_path: dbus::Path<'static> = proxy
        .method_call("org.gnome.Mutter.ScreenCast", "CreateSession", (args,))
        .map(|r: (dbus::Path<'static>,)| r.0)?;
    debug!("Mutter ScreenCast session: {}", session_path);

    let session_proxy = conn.with_proxy(
        "org.gnome.Mutter.ScreenCast",
        &session_path,
        Duration::from_millis(1000),
    );

    let mut props: PropMap = HashMap::new();
    props.insert("width".to_string(), Variant(Box::new(width as i32)));
    props.insert("height".to_string(), Variant(Box::new(height as i32)));
    props.insert("is-platform".to_string(), Variant(Box::new(true)));
    let cursor_mode = if capture_cursor { 2u32 } else { 1u32 };
    props.insert("cursor-mode".to_string(), Variant(Box::new(cursor_mode)));

    let stream_path: dbus::Path<'static> = session_proxy
        .method_call(
            "org.gnome.Mutter.ScreenCast.Session",
            "RecordVirtual",
            (props,),
        )
        .map(|r: (dbus::Path<'static>,)| r.0)?;
    debug!("Mutter RecordVirtual stream: {}", stream_path);

    let node_id = Arc::new(Mutex::new(None::<u32>));
    let node_id_clone = node_id.clone();

    let mut m = MatchRule::new();
    m.path = Some(stream_path);
    m.msg_type = Some(MessageType::Signal);
    m.interface = Some("org.gnome.Mutter.ScreenCast.Stream".into());
    m.member = Some("PipeWireStreamAdded".into());
    conn.add_match(m, move |(node,): (u32,), _c, _msg| {
        debug!("PipeWireStreamAdded: node_id={}", node);
        *node_id_clone.lock().unwrap() = Some(node);
        true
    })?;

    let _: () = session_proxy.method_call("org.gnome.Mutter.ScreenCast.Session", "Start", ())?;
    debug!("Mutter ScreenCast session started");

    for _ in 0..100 {
        conn.process(Duration::from_millis(100))?;
        if node_id.lock().unwrap().is_some() {
            break;
        }
    }

    let node_id = node_id
        .lock()
        .unwrap()
            .ok_or_else(|| DBusError("Failed to receive PipeWireStreamAdded signal".into()))?;

    let session = Arc::new(MutterSession {
        conn: Arc::new(conn),
        session_path,
    });

    Ok(MutterCapturable {
        session,
        node_id,
        width,
        height,
    })
}
