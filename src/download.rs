use std::path::Path;

use anyhow::{Context, Result, bail};
use futures::{StreamExt, stream};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use md5::Md5;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub enum ExpectedHash {
    Sha256(String),
    Md5(String),
}

#[derive(Debug)]
pub struct DownloadItem {
    pub url: String,
    pub filename: String,
    pub size: u64,
    pub hash: Option<ExpectedHash>,
}

impl DownloadItem {
    /// Package name shown next to the progress bar ("htop_3.4.1_amd64.deb" → "htop").
    fn display_name(&self) -> &str {
        self.filename.split('_').next().unwrap_or(&self.filename)
    }
}

fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "  {msg:<24!} {bytes:>10} {binary_bytes_per_sec:>12} [{bar:30.cyan/black}] {percent:>3}%",
    )
    .unwrap()
    .progress_chars("━╸ ")
}

fn total_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "  {msg:<24!} {bytes:>10} {binary_bytes_per_sec:>12} [{bar:30.green/black}] {percent:>3}%",
    )
    .unwrap()
    .progress_chars("━╸ ")
}

/// Download all items into `dest` (apt's archive cache), `jobs` at a time,
/// with a pacman-style progress display.
pub async fn download_all(items: &[DownloadItem], dest: &Path, jobs: usize) -> Result<()> {
    let partial_dir = dest.join("partial");
    std::fs::create_dir_all(&partial_dir)
        .with_context(|| format!("cannot create {}", partial_dir.display()))?;

    let client = reqwest::Client::builder()
        .user_agent(concat!("wrapt/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let multi = MultiProgress::new();
    let total = multi.add(ProgressBar::new(items.iter().map(|i| i.size).sum()));
    total.set_style(total_style());
    total.set_message("Total");

    let results: Vec<Result<()>> = stream::iter(items)
        .map(|item| {
            let client = client.clone();
            let multi = multi.clone();
            let total = total.clone();
            async move {
                download_one(&client, item, dest, &multi, &total)
                    .await
                    .with_context(|| format!("failed to download {}", item.filename))
            }
        })
        .buffer_unordered(jobs)
        .collect()
        .await;

    total.finish();
    for result in results {
        result?;
    }
    Ok(())
}

async fn download_one(
    client: &reqwest::Client,
    item: &DownloadItem,
    dest: &Path,
    multi: &MultiProgress,
    total: &ProgressBar,
) -> Result<()> {
    let final_path = dest.join(&item.filename);

    // Already in the cache with the right size — apt will hash-verify it anyway.
    if let Ok(meta) = std::fs::metadata(&final_path)
        && meta.len() == item.size
    {
        total.inc(item.size);
        return Ok(());
    }

    let bar = multi.insert_before(total, ProgressBar::new(item.size));
    bar.set_style(bar_style());
    bar.set_message(item.display_name().to_string());

    let mut response = client.get(&item.url).send().await?.error_for_status()?;

    let partial_path = dest.join("partial").join(&item.filename);
    let mut file = tokio::fs::File::create(&partial_path)
        .await
        .with_context(|| format!("cannot write {}", partial_path.display()))?;

    let mut sha256 = Sha256::new();
    let mut md5 = Md5::new();
    while let Some(chunk) = response.chunk().await? {
        match item.hash {
            Some(ExpectedHash::Sha256(_)) => sha256.update(&chunk),
            Some(ExpectedHash::Md5(_)) => md5.update(&chunk),
            None => {}
        }
        file.write_all(&chunk).await?;
        bar.inc(chunk.len() as u64);
        total.inc(chunk.len() as u64);
    }
    file.flush().await?;
    drop(file);

    let ok = match &item.hash {
        Some(ExpectedHash::Sha256(expected)) => hex::encode(sha256.finalize()) == *expected,
        Some(ExpectedHash::Md5(expected)) => hex::encode(md5.finalize()) == *expected,
        None => true,
    };
    if !ok {
        let _ = tokio::fs::remove_file(&partial_path).await;
        bail!("checksum mismatch (corrupted download?)");
    }

    tokio::fs::rename(&partial_path, &final_path).await?;
    bar.finish();
    Ok(())
}
