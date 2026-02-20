use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use gtk::glib::prelude::Cast;
use tracing::{debug, warn};

pub(super) type ImageLoadCallback = Box<dyn FnOnce(u64, Result<gtk::gdk::Texture, String>)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ImageRequestKind {
    Detail,
    GridThumb,
}

#[derive(Debug)]
struct ImageDecodeTask {
    id: u64,
    path: PathBuf,
    scale: Option<(i32, i32)>,
    kind: ImageRequestKind,
}

#[derive(Clone)]
struct DecodedImage {
    width: i32,
    height: i32,
    rowstride: usize,
    format: gtk::gdk::MemoryFormat,
    pixels: gtk::glib::Bytes,
}

enum ImageDecodeResult {
    Ok { id: u64, image: DecodedImage },
    Err { id: u64, message: String },
}

#[derive(Default)]
struct ImageTaskQueues {
    detail: VecDeque<ImageDecodeTask>,
    grid: VecDeque<ImageDecodeTask>,
}

#[derive(Clone, Copy, Debug)]
enum ImageWorkerLane {
    Detail,
    Grid,
}

#[derive(Clone)]
pub(super) struct ImageLoader {
    next_id: Rc<Cell<u64>>,
    callbacks: Rc<RefCell<HashMap<u64, ImageLoadCallback>>>,
    queue_state: Arc<(Mutex<ImageTaskQueues>, Condvar)>,
}

impl ImageLoader {
    pub(super) fn new() -> Self {
        let (result_tx, result_rx) = mpsc::channel::<ImageDecodeResult>();
        let queue_state = Arc::new((Mutex::new(ImageTaskQueues::default()), Condvar::new()));

        let callbacks = Rc::new(RefCell::new(HashMap::<u64, ImageLoadCallback>::new()));
        {
            let callbacks_handle = callbacks.clone();
            gtk::glib::timeout_add_local(Duration::from_millis(8), move || {
                while let Ok(message) = result_rx.try_recv() {
                    let id = match &message {
                        ImageDecodeResult::Ok { id, .. } => *id,
                        ImageDecodeResult::Err { id, .. } => *id,
                    };

                    let Some(callback) = callbacks_handle.borrow_mut().remove(&id) else {
                        continue;
                    };

                    match message {
                        ImageDecodeResult::Ok { image, .. } => {
                            let texture = gtk::gdk::MemoryTexture::new(
                                image.width,
                                image.height,
                                image.format,
                                &image.pixels,
                                image.rowstride,
                            );
                            callback(id, Ok(texture.upcast::<gtk::gdk::Texture>()));
                        }
                        ImageDecodeResult::Err { message, .. } => callback(id, Err(message)),
                    }
                }

                gtk::glib::ControlFlow::Continue
            });
        }

        spawn_image_worker(
            "booru-image-worker-detail",
            ImageWorkerLane::Detail,
            queue_state.clone(),
            result_tx.clone(),
        );
        spawn_image_worker(
            "booru-image-worker-grid-0",
            ImageWorkerLane::Grid,
            queue_state.clone(),
            result_tx.clone(),
        );
        spawn_image_worker(
            "booru-image-worker-grid-1",
            ImageWorkerLane::Grid,
            queue_state.clone(),
            result_tx,
        );

        Self {
            next_id: Rc::new(Cell::new(1)),
            callbacks,
            queue_state,
        }
    }

    pub(super) fn load<F>(
        &self,
        path: PathBuf,
        scale: Option<(i32, i32)>,
        kind: ImageRequestKind,
        callback: F,
    ) -> u64
    where
        F: FnOnce(u64, Result<gtk::gdk::Texture, String>) + 'static,
    {
        let id = self.next_id.get();
        self.next_id.set(id.wrapping_add(1));
        self.callbacks.borrow_mut().insert(id, Box::new(callback));

        let task = ImageDecodeTask {
            id,
            path,
            scale,
            kind,
        };
        {
            let (lock, condvar) = &*self.queue_state;
            let mut queues = lock.lock().expect("image queue mutex poisoned");

            match kind {
                ImageRequestKind::Detail => queues.detail.push_back(task),
                ImageRequestKind::GridThumb => queues.grid.push_back(task),
            }

            condvar.notify_all();
        }

        id
    }

    pub(super) fn cancel_if_queued(&self, id: u64) -> bool {
        let removed = {
            let (lock, _) = &*self.queue_state;
            let mut queues = lock.lock().expect("image queue mutex poisoned");
            remove_queued_task(&mut queues.detail, id) || remove_queued_task(&mut queues.grid, id)
        };

        if removed {
            self.callbacks.borrow_mut().remove(&id);
        }

        removed
    }
}

fn image_decode_worker(
    lane: ImageWorkerLane,
    queue_state: Arc<(Mutex<ImageTaskQueues>, Condvar)>,
    result_tx: mpsc::Sender<ImageDecodeResult>,
) {
    loop {
        let task = {
            let (lock, condvar) = &*queue_state;
            let mut queues = lock.lock().expect("image queue mutex poisoned");

            while queue_is_empty_for_lane(&queues, lane) {
                queues = condvar
                    .wait(queues)
                    .expect("image queue mutex poisoned while waiting");
            }

            pop_task_for_lane(&mut queues, lane).expect("worker lane queue unexpectedly empty")
        };

        debug!(lane = ?lane, kind = ?task.kind, path = %task.path.display(), "render");
        let outcome = decode_image_for_texture(&task.path, task.scale)
            .map(|image| ImageDecodeResult::Ok { id: task.id, image })
            .unwrap_or_else(|message| {
                warn!(
                    lane = ?lane,
                    kind = ?task.kind,
                    path = %task.path.display(),
                    error = %message,
                    "render failed"
                );
                ImageDecodeResult::Err {
                    id: task.id,
                    message,
                }
            });

        if result_tx.send(outcome).is_err() {
            break;
        }
    }
}

fn spawn_image_worker(
    name: &str,
    lane: ImageWorkerLane,
    queue_state: Arc<(Mutex<ImageTaskQueues>, Condvar)>,
    result_tx: mpsc::Sender<ImageDecodeResult>,
) {
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || image_decode_worker(lane, queue_state, result_tx))
        .expect("failed to start booru image worker thread");
}

fn queue_is_empty_for_lane(queues: &ImageTaskQueues, lane: ImageWorkerLane) -> bool {
    match lane {
        ImageWorkerLane::Detail => queues.detail.is_empty(),
        ImageWorkerLane::Grid => queues.grid.is_empty(),
    }
}

fn pop_task_for_lane(
    queues: &mut ImageTaskQueues,
    lane: ImageWorkerLane,
) -> Option<ImageDecodeTask> {
    match lane {
        ImageWorkerLane::Detail => queues.detail.pop_back(),
        ImageWorkerLane::Grid => queues.grid.pop_front(),
    }
}

fn remove_queued_task(queue: &mut VecDeque<ImageDecodeTask>, id: u64) -> bool {
    let Some(position) = queue.iter().position(|task| task.id == id) else {
        return false;
    };
    queue.remove(position);
    true
}

fn decode_image_for_texture(
    path: &PathBuf,
    scale: Option<(i32, i32)>,
) -> Result<DecodedImage, String> {
    let pixbuf = match scale {
        Some((width, height)) => {
            gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, width, height, true)
        }
        None => gtk::gdk_pixbuf::Pixbuf::from_file(path),
    }
    .map_err(|err| err.to_string())?;

    if pixbuf.colorspace() != gtk::gdk_pixbuf::Colorspace::Rgb {
        return Err("unsupported pixbuf colorspace".to_string());
    }
    if pixbuf.bits_per_sample() != 8 {
        return Err("unsupported bits-per-sample (expected 8)".to_string());
    }

    let format = match (pixbuf.has_alpha(), pixbuf.n_channels()) {
        (true, 4) => gtk::gdk::MemoryFormat::R8g8b8a8,
        (false, 3) => gtk::gdk::MemoryFormat::R8g8b8,
        (has_alpha, channels) => {
            return Err(format!(
                "unsupported channel layout (has_alpha={has_alpha}, channels={channels})"
            ));
        }
    };

    let rowstride =
        usize::try_from(pixbuf.rowstride()).map_err(|_| "invalid rowstride".to_string())?;
    Ok(DecodedImage {
        width: pixbuf.width(),
        height: pixbuf.height(),
        rowstride,
        format,
        pixels: pixbuf.read_pixel_bytes(),
    })
}
