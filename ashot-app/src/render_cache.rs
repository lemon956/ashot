use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};

use ashot_core::{
    Annotation,
    export::{encode_png_bytes, render_document_from_rgba, update_rendered_image},
};
use image::RgbaImage;
use tokio::runtime::Handle;

pub type RenderCacheCallback = Box<dyn FnOnce(std::result::Result<Arc<Vec<u8>>, String>) + 'static>;

#[derive(Debug, Clone, PartialEq)]
pub struct RenderCacheJob {
    pub revision: u64,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RenderCacheJobResult {
    pub revision: u64,
    pub annotations: Vec<Annotation>,
    pub png_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenderCacheStateAction {
    Start(RenderCacheJob),
    Ready,
    Wait,
}

#[derive(Default)]
pub struct RenderCacheState {
    next_revision: u64,
    in_flight: Option<RenderCacheJob>,
    pending: Option<Vec<Annotation>>,
    completed_annotations: Option<Vec<Annotation>>,
    completed_png_bytes: Option<Arc<Vec<u8>>>,
}

impl RenderCacheState {
    pub fn request_update(&mut self, annotations: Vec<Annotation>) -> RenderCacheStateAction {
        if self.has_completed_bytes_for(&annotations) {
            return RenderCacheStateAction::Ready;
        }
        if let Some(in_flight) = &self.in_flight {
            if in_flight.annotations != annotations {
                self.pending = Some(annotations);
            }
            return RenderCacheStateAction::Wait;
        }
        self.start_job(annotations)
    }

    pub fn complete(&mut self, result: RenderCacheJobResult) -> RenderCacheStateAction {
        let Some(in_flight) = &self.in_flight else {
            return RenderCacheStateAction::Wait;
        };
        if in_flight.revision != result.revision {
            return RenderCacheStateAction::Wait;
        }

        self.completed_annotations = Some(result.annotations.clone());
        self.completed_png_bytes = Some(Arc::new(result.png_bytes));
        self.in_flight = None;

        if let Some(pending) = self.pending.take()
            && !self.has_completed_bytes_for(&pending)
        {
            return self.start_job(pending);
        }
        RenderCacheStateAction::Ready
    }

    pub fn fail(&mut self, revision: u64) -> RenderCacheStateAction {
        if self.in_flight.as_ref().is_none_or(|job| job.revision != revision) {
            return RenderCacheStateAction::Wait;
        }
        self.in_flight = None;
        if let Some(pending) = self.pending.take() {
            return self.start_job(pending);
        }
        RenderCacheStateAction::Wait
    }

    pub fn has_completed_bytes_for(&self, annotations: &[Annotation]) -> bool {
        self.completed_annotations.as_deref() == Some(annotations)
            && self.completed_png_bytes.is_some()
    }

    pub fn completed_bytes_for(&self, annotations: &[Annotation]) -> Option<Arc<Vec<u8>>> {
        if self.completed_annotations.as_deref() == Some(annotations) {
            self.completed_png_bytes.clone()
        } else {
            None
        }
    }

    fn start_job(&mut self, annotations: Vec<Annotation>) -> RenderCacheStateAction {
        self.next_revision += 1;
        let job = RenderCacheJob { revision: self.next_revision, annotations };
        self.in_flight = Some(job.clone());
        RenderCacheStateAction::Start(job)
    }
}

struct RenderCacheWaiter {
    annotations: Vec<Annotation>,
    callback: Option<RenderCacheCallback>,
}

struct RenderCacheRendered {
    revision: u64,
    annotations: Vec<Annotation>,
    image: RgbaImage,
    png_bytes: Vec<u8>,
}

struct RenderCacheWorkerMessage {
    revision: u64,
    annotations: Vec<Annotation>,
    result: std::result::Result<RenderCacheRendered, String>,
}

pub struct RenderCache {
    state: RenderCacheState,
    runtime: Handle,
    base: Arc<RgbaImage>,
    completed_image: Option<RgbaImage>,
    tx: mpsc::Sender<RenderCacheWorkerMessage>,
    rx: mpsc::Receiver<RenderCacheWorkerMessage>,
    waiters: Vec<RenderCacheWaiter>,
}

impl RenderCache {
    pub fn new(runtime: Handle, base: Arc<RgbaImage>) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            state: RenderCacheState::default(),
            runtime,
            base,
            completed_image: None,
            tx,
            rx,
            waiters: Vec::new(),
        }
    }

    pub fn request_update(&mut self, annotations: Vec<Annotation>) {
        if self.pending_is_waited_on() {
            return;
        }
        let action = self.state.request_update(annotations);
        self.handle_action(action);
    }

    pub fn request_latest(&mut self, annotations: Vec<Annotation>, callback: RenderCacheCallback) {
        if let Some(bytes) = self.state.completed_bytes_for(&annotations) {
            callback(Ok(bytes));
            return;
        }

        self.waiters
            .push(RenderCacheWaiter { annotations: annotations.clone(), callback: Some(callback) });
        let action = self.state.request_update(annotations);
        self.handle_action(action);
    }

    pub fn poll(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            match message.result {
                Ok(rendered) => {
                    self.completed_image = Some(rendered.image);
                    let action = self.state.complete(RenderCacheJobResult {
                        revision: rendered.revision,
                        annotations: rendered.annotations.clone(),
                        png_bytes: rendered.png_bytes,
                    });
                    self.satisfy_waiters();
                    self.handle_action(action);
                }
                Err(error) => {
                    self.fail_waiters(&message.annotations, error);
                    let action = self.state.fail(message.revision);
                    self.handle_action(action);
                }
            }
        }
    }

    fn handle_action(&mut self, action: RenderCacheStateAction) {
        if let RenderCacheStateAction::Start(job) = action {
            self.start_worker(job);
        }
    }

    fn start_worker(&self, job: RenderCacheJob) {
        let tx = self.tx.clone();
        let base = self.base.clone();
        let old_annotations = self.state.completed_annotations.clone().unwrap_or_default();
        let mut cached =
            self.completed_image.clone().unwrap_or_else(|| render_document_from_rgba(&base, &[]));
        self.runtime.spawn_blocking(move || {
            update_rendered_image(&base, &mut cached, &old_annotations, &job.annotations);
            let result = encode_png_bytes(&cached)
                .map(|png_bytes| RenderCacheRendered {
                    revision: job.revision,
                    annotations: job.annotations.clone(),
                    image: cached,
                    png_bytes,
                })
                .map_err(|source| format!("failed to render PNG cache: {source}"));
            let _ = tx.send(RenderCacheWorkerMessage {
                revision: job.revision,
                annotations: job.annotations,
                result,
            });
        });
    }

    fn satisfy_waiters(&mut self) {
        let mut remaining = Vec::new();
        for mut waiter in self.waiters.drain(..) {
            if let Some(bytes) = self.state.completed_bytes_for(&waiter.annotations) {
                if let Some(callback) = waiter.callback.take() {
                    callback(Ok(bytes));
                }
            } else {
                remaining.push(waiter);
            }
        }
        self.waiters = remaining;
    }

    fn fail_waiters(&mut self, annotations: &[Annotation], error: String) {
        let mut remaining = Vec::new();
        for mut waiter in self.waiters.drain(..) {
            if waiter.annotations == annotations {
                if let Some(callback) = waiter.callback.take() {
                    callback(Err(error.clone()));
                }
            } else {
                remaining.push(waiter);
            }
        }
        self.waiters = remaining;
    }

    fn pending_is_waited_on(&self) -> bool {
        self.state
            .pending
            .as_ref()
            .is_some_and(|pending| self.waiters.iter().any(|waiter| waiter.annotations == *pending))
    }
}

pub fn save_png_bytes_to_dir_with_filename(
    save_dir: &Path,
    png_bytes: &[u8],
    requested_filename: &str,
) -> std::result::Result<PathBuf, String> {
    let filename = normalized_png_filename(requested_filename)
        .ok_or_else(|| "Enter a file name".to_string())?;
    std::fs::create_dir_all(save_dir).map_err(|source| {
        format!("failed to create screenshot directory {}: {source}", save_dir.display())
    })?;
    let output = save_dir.join(filename);
    std::fs::write(&output, png_bytes)
        .map_err(|source| format!("failed to save screenshot at {}: {source}", output.display()))?;
    Ok(output)
}

fn normalized_png_filename(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut path = PathBuf::from(trimmed);
    if path.extension().is_none_or(|extension| extension != "png" && extension != "PNG") {
        path.set_extension("png");
    }
    path.file_name().map(|name| name.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        RenderCacheJobResult, RenderCacheState, RenderCacheStateAction,
        save_png_bytes_to_dir_with_filename,
    };
    use ashot_core::{Annotation, AnnotationData, Color, Point};

    fn line_annotation(end_x: f32) -> Annotation {
        Annotation::new(AnnotationData::Line {
            start: Point::new(1.0, 1.0),
            end: Point::new(end_x, 8.0),
            color: Color::rgba(255, 0, 0, 255),
            stroke_width: 2,
        })
    }

    #[test]
    fn render_cache_state_queues_latest_pending_request() {
        let mut state = RenderCacheState::default();
        let first = vec![line_annotation(8.0)];
        let second = vec![line_annotation(16.0)];
        let third = vec![line_annotation(24.0)];

        let RenderCacheStateAction::Start(first_job) = state.request_update(first) else {
            panic!("first request should start a job");
        };
        assert_eq!(first_job.revision, 1);

        assert_eq!(state.request_update(second.clone()), RenderCacheStateAction::Wait);
        assert_eq!(state.request_update(third.clone()), RenderCacheStateAction::Wait);

        let action = state.complete(RenderCacheJobResult {
            revision: first_job.revision,
            annotations: first_job.annotations,
            png_bytes: vec![1],
        });

        let RenderCacheStateAction::Start(next_job) = action else {
            panic!("latest pending request should start after current job completes");
        };
        assert_eq!(next_job.annotations, third);
    }

    #[test]
    fn render_cache_state_ignores_stale_results() {
        let mut state = RenderCacheState::default();
        let first = vec![line_annotation(8.0)];
        let RenderCacheStateAction::Start(first_job) = state.request_update(first.clone()) else {
            panic!("first request should start");
        };

        assert_eq!(
            state.complete(RenderCacheJobResult {
                revision: first_job.revision + 1,
                annotations: first,
                png_bytes: vec![9],
            }),
            RenderCacheStateAction::Wait
        );
        assert!(!state.has_completed_bytes_for(&first_job.annotations));
    }

    #[test]
    fn save_png_bytes_to_dir_normalizes_filename() {
        let dir =
            std::env::temp_dir().join(format!("ashot-render-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let output = save_png_bytes_to_dir_with_filename(&dir, b"fake png bytes", "example.jpg")
            .expect("write bytes");

        assert_eq!(output, dir.join(Path::new("example.png")));
        assert_eq!(std::fs::read(output).expect("read output"), b"fake png bytes");
        let _ = std::fs::remove_dir_all(dir);
    }
}
