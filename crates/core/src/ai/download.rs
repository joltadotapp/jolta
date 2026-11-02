//! Downloading a local model: resumable, into `<data dir>/models`, checked
//! against its SHA-256 before it counts as ready.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::Cancel;
use super::local::LocalModel;
use crate::{Error, Result};

pub fn path(data_dir: &Path, model: &LocalModel) -> PathBuf {
    data_dir.join("models").join(model.file)
}

fn part_path(data_dir: &Path, model: &LocalModel) -> PathBuf {
    data_dir.join("models").join(format!("{}.part", model.file))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Missing,
    /// Stopped partway, with this many bytes on disk.
    Partial(u64),
    Ready,
}

pub fn state(data_dir: &Path, model: &LocalModel) -> State {
    if path(data_dir, model).exists() {
        return State::Ready;
    }
    match std::fs::metadata(part_path(data_dir, model)) {
        Ok(m) => State::Partial(m.len()),
        Err(_) => State::Missing,
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Step {
    Downloading { done: u64, total: u64, bytes_per_second: f64 },
    Verifying,
}

/// Downloads the model, carrying on from a partial file. Returns false when
/// cancelled, keeping what arrived so the next call resumes.
pub async fn download(data_dir: &Path, model: &LocalModel, cancel: &Cancel, on: &mut (dyn FnMut(Step) + Send)) -> Result<bool> {
    let part = part_path(data_dir, model);
    std::fs::create_dir_all(part.parent().expect("models folder"))?;
    let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if have > model.size {
        std::fs::remove_file(&part)?;
        have = 0;
    }
    if have < model.size {
        crate::oauth::init_tls();
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| Error::Other(format!("HTTP client: {e}")))?;
        let mut request = client.get(model.url);
        if have > 0 {
            request = request.header("Range", format!("bytes={have}-"));
        }
        let mut response = tokio::select! {
            r = request.send() => r.map_err(|e| Error::Other(format!("Couldn't reach Hugging Face: {e}")))?,
            _ = cancel.cancelled() => return Ok(false),
        };
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Other(format!("Hugging Face refused the download ({status}).")));
        }
        // A server that ignores the range sends the whole file again.
        if have > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            have = 0;
        }
        let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(have == 0).append(have > 0).open(&part)?;
        let mut done = have;
        let mut window = (Instant::now(), done);
        let mut speed = 0.0;
        let mut last_report = Instant::now() - Duration::from_secs(1);
        loop {
            let chunk = tokio::select! {
                c = response.chunk() => c.map_err(|e| Error::Other(format!("The download stopped: {e}")))?,
                _ = cancel.cancelled() => return Ok(false),
            };
            let Some(chunk) = chunk else { break };
            file.write_all(&chunk).map_err(|e| match e.raw_os_error() {
                Some(28) => Error::Other("Not enough free disk space for the model.".into()),
                _ => e.into(),
            })?;
            done += chunk.len() as u64;
            if last_report.elapsed() >= Duration::from_millis(250) {
                let seconds = window.0.elapsed().as_secs_f64();
                if seconds >= 1.0 {
                    let now = (done - window.1) as f64 / seconds;
                    speed = if speed == 0.0 { now } else { speed * 0.7 + now * 0.3 };
                    window = (Instant::now(), done);
                }
                on(Step::Downloading { done, total: model.size, bytes_per_second: speed });
                last_report = Instant::now();
            }
        }
        file.flush()?;
        if done < model.size {
            return Err(Error::Other("The download ended early. Try again to carry on.".into()));
        }
    }
    on(Step::Verifying);
    let check = part.clone();
    let hash = tokio::task::spawn_blocking(move || -> Result<String> {
        let mut file = std::fs::File::open(check)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0; 1 << 20];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    })
    .await??;
    if hash != model.sha256 {
        std::fs::remove_file(&part)?;
        return Err(Error::Other("The downloaded file was damaged, so Kite deleted it. Try again.".into()));
    }
    std::fs::rename(&part, path(data_dir, model))?;
    Ok(true)
}

/// Deletes the model and any partial download.
pub fn remove(data_dir: &Path, model: &LocalModel) -> Result<()> {
    for p in [path(data_dir, model), part_path(data_dir, model)] {
        match std::fs::remove_file(p) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(())
}
