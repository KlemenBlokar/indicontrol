//! Single-camera capture: prepare equipment, expose, download, process.

use std::time::Duration;

use axum::body::Bytes;
use protocol::equipment::{self, Control};
use protocol::preview_frame::{self, Kind};
use protocol::{CaptureRequest, CaptureState, LogLevel, ToUi};

use super::{Core, CoreMsg, Outgoing};
use crate::frame::{self, ProcessInput, ProcessOutput};
use crate::metadata::{self, FrameContext};

/// Extra time allowed after the exposure for download before giving up.
const DOWNLOAD_TIMEOUT: f64 = 120.0;
const FILTER_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) struct Job {
    id: u64,
    req: CaptureRequest,
    camera: String,
    wheel: Option<String>,
    state: CaptureState,
    frames: u32,
    ctx: Option<FrameContext>,
    /// Last broadcast remaining time, to throttle progress updates.
    last_remaining: f64,
    /// Started by the sequencer, which is told about the outcome.
    from_sequence: bool,
}

impl Core {
    pub(super) fn start_capture(&mut self, req: CaptureRequest, from_sequence: bool) -> Result<(), String> {
        if self.job.is_some() {
            return Err("a capture is already running".into());
        }
        if !from_sequence && self.run.is_some() {
            return Err("a sequence is running".into());
        }
        if !(req.exposure >= 0.0 && req.exposure.is_finite()) {
            return Err("invalid exposure time".into());
        }
        let camera = self
            .state
            .roles
            .main_camera
            .clone()
            .ok_or("assign a main camera in Equipment first")?;
        let client = self.client.as_ref().ok_or("not connected to an INDI server")?;
        let (wheel, needs_filter_change) = {
            let cache = client.cache();
            let cam = cache
                .devices
                .get(&camera)
                .filter(|d| d.is_connected())
                .ok_or_else(|| format!("camera '{camera}' is not connected"))?;
            if !cam.properties.contains_key("CCD_EXPOSURE") {
                return Err(format!("'{camera}' has no CCD_EXPOSURE property"));
            }

            let e = |r: Result<(), indi_link::ClientError>| r.map_err(|e| e.to_string());
            e(client.enable_blob(&camera, None, indi_link::BlobMode::Also))?;
            if cam
                .properties
                .get("UPLOAD_MODE")
                .is_some_and(|p| p.switch("UPLOAD_CLIENT") == Some(false) && p.switch("UPLOAD_BOTH") != Some(true))
            {
                e(client.send_switch(&camera, "UPLOAD_MODE", &[("UPLOAD_CLIENT", true)]))?;
            }
            if cam.properties.contains_key("CCD_FRAME_TYPE") {
                e(client.send_switch(&camera, "CCD_FRAME_TYPE", &[(req.frame_type.indi_switch(), true)]))?;
            }
            if let Some(b) = req.binning
                && cam.properties.contains_key("CCD_BINNING")
            {
                let b = b as f64;
                e(client.send_number(&camera, "CCD_BINNING", &[("HOR_BIN", b), ("VER_BIN", b)]))?;
            }
            for (control, value) in [(Control::Gain, req.gain), (Control::Offset, req.offset)] {
                if let Some(v) = value {
                    let (prop, el) = equipment::control_location(cam, control)
                        .ok_or_else(|| format!("'{camera}' has no {control:?} control"))?;
                    e(client.send_number(&camera, &prop, &[(&el, v)]))?;
                }
            }

            let wheel = self.state.roles.filter_wheel.clone();
            let mut change = false;
            if let Some(slot) = req.filter_slot {
                let name = wheel.as_deref().ok_or("no filter wheel assigned")?;
                let w = cache
                    .devices
                    .get(name)
                    .filter(|d| d.is_connected())
                    .ok_or_else(|| format!("filter wheel '{name}' is not connected"))?;
                if equipment::current_filter_slot(w) != Some(slot) {
                    e(client.send_number(name, "FILTER_SLOT", &[("FILTER_SLOT_VALUE", slot as f64)]))?;
                    change = true;
                }
            }
            (wheel, change)
        };

        self.next_job_id += 1;
        self.job = Some(Job {
            id: self.next_job_id,
            req,
            camera,
            wheel,
            state: CaptureState::Idle,
            frames: 0,
            ctx: None,
            last_remaining: f64::INFINITY,
            from_sequence,
        });
        if needs_filter_change {
            self.set_capture_state(CaptureState::Preparing {
                what: "Changing filter".into(),
            });
            let (id, tx) = (self.next_job_id, self.tx.clone());
            tokio::spawn(async move {
                tokio::time::sleep(FILTER_TIMEOUT).await;
                let _ = tx.send(CoreMsg::FilterTimeout { job_id: id });
            });
            Ok(())
        } else {
            self.begin_exposure()
        }
    }

    fn begin_exposure(&mut self) -> Result<(), String> {
        let (Some(job), Some(client)) = (&mut self.job, &self.client) else {
            return Err("capture was cancelled".into());
        };
        job.ctx = Some(metadata::collect(
            &client.cache(),
            &self.state.roles,
            &job.req,
            &job.camera,
            chrono::Utc::now(),
        ));
        client
            .send_number(&job.camera, "CCD_EXPOSURE", &[("CCD_EXPOSURE_VALUE", job.req.exposure)])
            .map_err(|e| e.to_string())?;
        job.last_remaining = f64::INFINITY;
        let (id, frame, total) = (job.id, job.frames, job.req.exposure);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(total + DOWNLOAD_TIMEOUT)).await;
            let _ = tx.send(CoreMsg::CaptureTimeout { job_id: id, frame });
        });
        self.set_capture_state(CaptureState::Exposing {
            total,
            remaining: total,
        });
        Ok(())
    }

    /// React to property changes of the camera / filter wheel.
    pub(super) fn capture_on_update(&mut self, device: &str, name: &str) {
        let Some(job) = &self.job else {
            return;
        };
        let Some(prop) = self.property(device, name) else {
            return;
        };
        use indi_link::PropertyState as S;
        match (&job.state, name) {
            (CaptureState::Preparing { .. }, "FILTER_SLOT") if job.wheel.as_deref() == Some(device) => {
                let target = job.req.filter_slot.map(f64::from);
                match prop.state {
                    S::Alert => self.fail_capture("filter wheel reported an error".into()),
                    S::Ok | S::Idle if prop.number("FILTER_SLOT_VALUE").map(f64::round) == target => {
                        if let Err(e) = self.begin_exposure() {
                            self.fail_capture(e);
                        }
                    }
                    _ => {}
                }
            }
            (CaptureState::Exposing { total, .. }, "CCD_EXPOSURE") if job.camera == device => {
                let total = *total;
                let remaining = prop.number("CCD_EXPOSURE_VALUE").unwrap_or(0.0);
                match prop.state {
                    S::Alert => self.fail_capture("camera reported an exposure failure".into()),
                    S::Ok => self.set_capture_state(CaptureState::Downloading),
                    S::Busy => {
                        let job = self.job.as_mut().expect("checked above");
                        if (job.last_remaining - remaining).abs() >= 1.0 || remaining == 0.0 {
                            job.last_remaining = remaining;
                            self.set_capture_state(CaptureState::Exposing { total, remaining });
                        }
                    }
                    S::Idle => {}
                }
            }
            _ => {}
        }
    }

    pub(super) fn capture_on_blob(&mut self, device: &str, format: String, blob: std::sync::Arc<Vec<u8>>) {
        let Some(job) = &mut self.job else {
            return;
        };
        if job.camera != device
            || !matches!(job.state, CaptureState::Exposing { .. } | CaptureState::Downloading)
        {
            return;
        }
        let Some(ctx) = job.ctx.take() else {
            return;
        };
        self.next_frame_id += 1;
        let input = ProcessInput {
            frame_id: self.next_frame_id,
            blob,
            format,
            ctx,
            settings: self.state.imaging.clone(),
            save: job.req.save,
        };
        let (job_id, tx) = (job.id, self.tx.clone());
        self.set_capture_state(CaptureState::Processing);
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || frame::process(input))
                .await
                .unwrap_or_else(|e| Err(format!("frame processing crashed: {e}")));
            let _ = tx.send(CoreMsg::FrameDone {
                job_id,
                result: Box::new(result),
            });
        });
    }

    pub(super) fn frame_done(&mut self, job_id: u64, result: Result<ProcessOutput, String>) {
        let current = self.job.as_ref().is_some_and(|j| j.id == job_id);
        match result {
            Ok(out) => {
                let a = &out.info.analysis;
                let brightest = a
                    .channels
                    .iter()
                    .map(|c| c.median)
                    .fold(0.0, f64::max)
                    / a.adc_max.max(1) as f64;
                let text = match &out.info.path {
                    Some(p) => format!("Saved {p}"),
                    None => format!("Captured {} ({}×{})", out.info.camera, out.info.width, out.info.height),
                };
                self.publish_frame(out);
                self.log(None, text, LogLevel::Info);
                if !current {
                    return;
                }
                let job = self.job.as_mut().expect("current");
                job.frames += 1;
                if job.req.repeat {
                    if let Err(e) = self.begin_exposure() {
                        self.fail_capture(e);
                    }
                } else {
                    let from_sequence = job.from_sequence;
                    self.finish_capture();
                    if from_sequence {
                        self.sequence_frame_done(brightest);
                    }
                }
            }
            Err(e) => {
                if current {
                    self.fail_capture(e);
                } else {
                    self.log(None, e, LogLevel::Error);
                }
            }
        }
    }

    fn publish_frame(&mut self, out: ProcessOutput) {
        let id = out.info.id;
        let info = Outgoing::text(&ToUi::Frame(Box::new(out.info)));
        let preview = Outgoing::Binary(Bytes::from(preview_frame::encode(Kind::Main, id, &out.jpeg)));
        self.broadcast(info.clone());
        self.broadcast(preview.clone());
        self.last_frame = Some((info, preview));
    }

    pub(super) fn abort_capture(&mut self) {
        let Some(job) = &self.job else {
            return;
        };
        if matches!(job.state, CaptureState::Exposing { .. })
            && let Some(c) = &self.client
        {
            let _ = c.send_switch(&job.camera, "CCD_ABORT_EXPOSURE", &[("ABORT", true)]);
        }
        self.log(None, "Capture aborted", LogLevel::Warning);
        self.finish_capture();
    }

    pub(super) fn capture_timeout(&mut self, job_id: u64, frame: u32) {
        if let Some(job) = &self.job
            && job.id == job_id
            && job.frames == frame
            && matches!(job.state, CaptureState::Exposing { .. } | CaptureState::Downloading)
        {
            self.fail_capture("no image received from the camera".into());
        }
    }

    pub(super) fn filter_timeout(&mut self, job_id: u64) {
        if let Some(job) = &self.job
            && job.id == job_id
            && matches!(job.state, CaptureState::Preparing { .. })
        {
            self.fail_capture("filter wheel did not reach the requested slot".into());
        }
    }

    pub(super) fn fail_capture(&mut self, reason: String) {
        if let Some(job) = &self.job {
            let from_sequence = job.from_sequence;
            self.log(None, format!("Capture failed: {reason}"), LogLevel::Error);
            self.finish_capture();
            if from_sequence {
                self.sequence_failed(reason);
            }
        }
    }

    fn finish_capture(&mut self) {
        let frames = self.job.as_ref().map_or(0, |j| j.frames);
        self.job = None;
        self.send(ToUi::Capture {
            state: CaptureState::Idle,
            frames,
        });
    }

    fn set_capture_state(&mut self, state: CaptureState) {
        let Some(job) = &mut self.job else {
            return;
        };
        job.state = state.clone();
        let frames = job.frames;
        self.send(ToUi::Capture { state, frames });
    }

    pub(super) fn capture_msg(&self) -> ToUi {
        match &self.job {
            Some(j) => ToUi::Capture {
                state: j.state.clone(),
                frames: j.frames,
            },
            None => ToUi::Capture {
                state: CaptureState::Idle,
                frames: 0,
            },
        }
    }
}
