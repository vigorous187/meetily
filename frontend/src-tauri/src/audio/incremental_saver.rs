use super::encode::encode_single_audio;
use super::recording_state::AudioChunk;
use anyhow::{anyhow, Result};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Runtime};

use super::ffmpeg::find_ffmpeg_path;

/// Audio data without device type (we only store mixed audio)
#[derive(Clone)]
struct AudioData {
    data: Vec<f32>,
    // sample_rate: u32,
}

/// Incremental audio saver that writes checkpoints every 30 seconds
/// to minimize memory usage and enable crash recovery
pub struct IncrementalAudioSaver {
    checkpoint_buffer: Vec<AudioData>,
    checkpoint_interval_samples: usize, // 30s at 48kHz = 1,440,000 samples
    checkpoint_count: u32,
    checkpoints_dir: PathBuf,
    meeting_folder: PathBuf,
    sample_rate: u32,
}

impl IncrementalAudioSaver {
    /// Create a new incremental saver
    ///
    /// # Arguments
    /// * `meeting_folder` - Path to the meeting folder (contains .checkpoints/)
    /// * `sample_rate` - Sample rate of audio (typically 48000)
    pub fn new(meeting_folder: PathBuf, sample_rate: u32) -> Result<Self> {
        let checkpoints_dir = meeting_folder.join(".checkpoints");

        // Verify checkpoints directory exists
        if !checkpoints_dir.exists() {
            return Err(anyhow!(
                "Checkpoints directory does not exist: {}",
                checkpoints_dir.display()
            ));
        }

        Ok(Self {
            checkpoint_buffer: Vec::new(),
            checkpoint_interval_samples: sample_rate as usize * 30, // 30 seconds
            checkpoint_count: 0,
            checkpoints_dir,
            meeting_folder,
            sample_rate,
        })
    }

    /// Add an audio chunk to the buffer
    /// Automatically saves a checkpoint when buffer reaches 30 seconds
    pub fn add_chunk(&mut self, chunk: AudioChunk) -> Result<()> {
        let audio_data = AudioData {
            data: chunk.data,
            // sample_rate: chunk.sample_rate,
        };

        self.checkpoint_buffer.push(audio_data);

        // Calculate total samples in buffer
        let total_samples: usize = self.checkpoint_buffer.iter().map(|c| c.data.len()).sum();

        // Save checkpoint when buffer reaches threshold (30 seconds)
        if total_samples >= self.checkpoint_interval_samples {
            self.save_checkpoint()?;
            self.checkpoint_buffer.clear();
        }

        Ok(())
    }

    /// Save current buffer as a checkpoint file
    fn save_checkpoint(&mut self) -> Result<()> {
        // Concatenate all chunks in buffer
        let audio_data: Vec<f32> = self
            .checkpoint_buffer
            .iter()
            .flat_map(|c| &c.data)
            .cloned()
            .collect();

        if audio_data.is_empty() {
            warn!("Attempted to save empty checkpoint, skipping");
            return Ok(());
        }

        // Generate checkpoint filename
        let checkpoint_path = self
            .checkpoints_dir
            .join(format!("audio_chunk_{:03}.mp4", self.checkpoint_count));

        // Encode and save checkpoint
        encode_single_audio(
            bytemuck::cast_slice(&audio_data),
            self.sample_rate,
            1, // mono
            &checkpoint_path,
        )?;

        let duration_seconds = audio_data.len() as f32 / self.sample_rate as f32;
        self.checkpoint_count += 1;

        info!(
            "Saved checkpoint {}: {:.2}s of audio ({} samples)",
            self.checkpoint_count,
            duration_seconds,
            audio_data.len()
        );

        Ok(())
    }

    /// Finalize the recording: save final checkpoint, merge all checkpoints, cleanup
    ///
    /// Returns the path to the final merged audio.mp4 file
    pub async fn finalize(&mut self) -> Result<PathBuf> {
        info!("Finalizing incremental recording...");

        // Save final buffer if not empty
        if !self.checkpoint_buffer.is_empty() {
            info!(
                "Saving final checkpoint with remaining {} chunks",
                self.checkpoint_buffer.len()
            );
            self.save_checkpoint()?;
            self.checkpoint_buffer.clear();
        }

        if self.checkpoint_count == 0 {
            return Err(anyhow!(
                "No audio checkpoints to merge - recording may have failed"
            ));
        }

        // Merge all checkpoints using FFmpeg concat
        let final_audio_path = self.meeting_folder.join("audio.mp4");
        self.merge_checkpoints(&final_audio_path).await?;

        // Clean up checkpoints directory
        info!("Cleaning up {} checkpoint files", self.checkpoint_count);
        if let Err(e) = std::fs::remove_dir_all(&self.checkpoints_dir) {
            warn!("Failed to clean up checkpoints directory: {}", e);
            // Non-fatal - user can manually delete
        }

        info!("Finalized recording: {}", final_audio_path.display());

        Ok(final_audio_path)
    }

    /// Merge all checkpoint files into final audio.mp4 using FFmpeg concat
    /// Uses concat demuxer for fast merging without re-encoding
    async fn merge_checkpoints(&self, output: &PathBuf) -> Result<()> {
        info!(
            "Merging {} checkpoints into final audio file...",
            self.checkpoint_count
        );

        // Create concat list file for FFmpeg
        let list_file = self.checkpoints_dir.join("concat_list.txt");
        let mut list_content = String::new();

        for i in 0..self.checkpoint_count {
            let checkpoint_path = self
                .checkpoints_dir
                .join(format!("audio_chunk_{:03}.mp4", i));

            // Verify checkpoint exists
            if !checkpoint_path.exists() {
                return Err(anyhow!(
                    "Checkpoint file missing: {}",
                    checkpoint_path.display()
                ));
            }

            // Use absolute path for FFmpeg (required for safe mode)
            let abs_path = checkpoint_path.canonicalize()?;
            list_content.push_str(&format!("file '{}'\n", abs_path.display()));
        }

        std::fs::write(&list_file, list_content)?;
        crate::path_security::harden_private_file(&list_file).map_err(anyhow::Error::msg)?;

        let ffmpeg_path = find_ffmpeg_path().ok_or_else(|| {
            anyhow!("FFmpeg not found. Please install FFmpeg to finalize recordings.")
        })?;
        info!("Using FFmpeg at: {:?}", ffmpeg_path);

        // Run FFmpeg concat command
        // Using concat demuxer with copy codec for fast merging (no re-encoding)

        let mut command = std::process::Command::new(ffmpeg_path);

        command.args(&[
            "-f",
            "concat", // Use concat demuxer
            "-safe",
            "0", // Allow absolute paths
            "-i",
            list_file.to_str().unwrap(),
            "-c",
            "copy", // Copy codec - no re-encoding!
            "-y",   // Overwrite output file
            output.to_str().unwrap(),
        ]);

        // Hide console window on Windows to prevent CMD popup during finalization
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let ffmpeg_output = command.output()?;

        if !ffmpeg_output.status.success() {
            let stderr = String::from_utf8_lossy(&ffmpeg_output.stderr);
            error!("FFmpeg merge failed: {}", stderr);
            return Err(anyhow!("FFmpeg concat failed: {}", stderr));
        }

        // Verify output file was created
        if !output.exists() {
            return Err(anyhow!(
                "Merged audio file was not created: {}",
                output.display()
            ));
        }
        crate::path_security::harden_private_file(output).map_err(anyhow::Error::msg)?;

        info!(
            "Successfully merged {} checkpoints → {}",
            self.checkpoint_count,
            output.display()
        );

        Ok(())
    }

    /// Get the meeting folder path
    pub fn get_meeting_folder(&self) -> &PathBuf {
        &self.meeting_folder
    }

    /// Get current checkpoint count
    pub fn get_checkpoint_count(&self) -> u32 {
        self.checkpoint_count
    }
}

/// Audio recovery status for transcript recovery feature
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioRecoveryStatus {
    pub status: String, // "success" | "partial" | "failed" | "none"
    pub chunk_count: u32,
    pub estimated_duration_seconds: f64,
    pub audio_file_path: Option<String>,
    pub message: String,
}

fn is_checkpoint_file_name(name: &str) -> bool {
    name.strip_prefix("audio_chunk_")
        .and_then(|value| value.strip_suffix(".mp4"))
        .is_some_and(|index| {
            !index.is_empty() && index.len() <= 6 && index.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn ffconcat_safe_path(path: &Path) -> Result<&str, String> {
    let value = path
        .to_str()
        .ok_or_else(|| "Checkpoint path is not valid UTF-8".to_string())?;
    if value
        .chars()
        .any(|character| matches!(character, '\n' | '\r' | '\'' | '\\'))
    {
        return Err("Checkpoint path contains characters unsafe for FFmpeg concat".to_string());
    }
    Ok(value)
}

fn validate_checkpoint_file(path: &Path, checkpoints_dir: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "Checkpoint file name is invalid".to_string())?;
    if !is_checkpoint_file_name(name) {
        return Err("Checkpoint file name is not approved".to_string());
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect checkpoint file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("Checkpoint must be a regular non-symlink file".to_string());
    }
    if metadata.len() > 128 * 1024 * 1024 {
        return Err("Checkpoint exceeds the 128 MiB size limit".to_string());
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("Could not verify checkpoint file: {error}"))?;
    if canonical.parent() != Some(checkpoints_dir) {
        return Err("Checkpoint resolved outside its approved directory".to_string());
    }
    Ok(canonical)
}

fn no_audio_recovery(message: &str) -> AudioRecoveryStatus {
    AudioRecoveryStatus {
        status: "none".to_string(),
        chunk_count: 0,
        estimated_duration_seconds: 0.0,
        audio_file_path: None,
        message: message.to_string(),
    }
}

fn validated_checkpoint_directory<R: Runtime>(
    app: &AppHandle<R>,
    meeting_folder: &str,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let folder_path =
        crate::path_security::validate_existing_approved_directory(app, Path::new(meeting_folder))?;
    let checkpoints_path = folder_path.join(".checkpoints");
    match std::fs::symlink_metadata(&checkpoints_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((folder_path, None)),
        Err(error) => Err(format!("Could not inspect checkpoints directory: {error}")),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err("Checkpoints path must be a regular non-symlink directory".to_string())
        }
        Ok(_) => {
            let checkpoints_path =
                crate::path_security::validate_existing_approved_directory(app, &checkpoints_path)?;
            if checkpoints_path.parent() != Some(folder_path.as_path())
                || checkpoints_path
                    .file_name()
                    .and_then(|value| value.to_str())
                    != Some(".checkpoints")
            {
                return Err("Checkpoints directory escaped the meeting folder".to_string());
            }
            Ok((folder_path, Some(checkpoints_path)))
        }
    }
}

#[tauri::command]
pub async fn recover_audio_from_checkpoints<R: Runtime>(
    app: AppHandle<R>,
    meeting_folder: String,
    _sample_rate: u32,
) -> Result<AudioRecoveryStatus, String> {
    let (folder_path, Some(checkpoints_dir)) =
        validated_checkpoint_directory(&app, &meeting_folder)?
    else {
        return Ok(no_audio_recovery("No audio checkpoints found"));
    };

    let mut checkpoint_files = Vec::new();
    for entry in std::fs::read_dir(&checkpoints_dir)
        .map_err(|error| format!("Failed to read checkpoints directory: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("Failed to inspect checkpoint entry: {error}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_checkpoint_file_name(name) {
            checkpoint_files.push(validate_checkpoint_file(&entry.path(), &checkpoints_dir)?);
        }
    }
    if checkpoint_files.is_empty() {
        return Ok(no_audio_recovery("No audio checkpoint files found"));
    }
    if checkpoint_files.len() > 2_000 {
        return Err("Too many checkpoint files to recover safely".to_string());
    }
    checkpoint_files.sort();

    let chunk_count = checkpoint_files.len() as u32;
    let estimated_duration = (chunk_count as f64) * 30.0;
    let concat_file_path =
        crate::path_security::validate_new_file(&app, &checkpoints_dir.join("concat_list.txt"))?;
    let mut concat_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&concat_file_path)
        .map_err(|error| format!("Failed to create concat list: {error}"))?;
    for path in &checkpoint_files {
        writeln!(concat_file, "file '{}'", ffconcat_safe_path(path)?)
            .map_err(|error| format!("Failed to write concat list: {error}"))?;
    }
    drop(concat_file);
    crate::path_security::harden_private_file(&concat_file_path)?;

    let output_path =
        crate::path_security::validate_new_file(&app, &folder_path.join("audio.mp4"))?;
    let ffmpeg_path =
        find_ffmpeg_path().ok_or_else(|| "FFmpeg is unavailable for audio recovery".to_string())?;
    let mut command = std::process::Command::new(ffmpeg_path);
    command
        .arg("-f")
        .arg("concat")
        .arg("-safe")
        .arg("0")
        .arg("-i")
        .arg(&concat_file_path)
        .arg("-c")
        .arg("copy")
        .arg("-n")
        .arg(&output_path);

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let ffmpeg_result = command.output();
    let _ = std::fs::remove_file(&concat_file_path);
    match ffmpeg_result {
        Ok(output) if output.status.success() => {
            crate::path_security::harden_private_file(&output_path)?;
            Ok(AudioRecoveryStatus {
                status: "success".to_string(),
                chunk_count,
                estimated_duration_seconds: estimated_duration,
                audio_file_path: Some(output_path.to_string_lossy().into_owned()),
                message: format!("Successfully recovered {} audio chunks", chunk_count),
            })
        }
        Ok(output) => {
            error!("FFmpeg recovery failed with status {}", output.status);
            Ok(AudioRecoveryStatus {
                status: "failed".to_string(),
                chunk_count,
                estimated_duration_seconds: estimated_duration,
                audio_file_path: None,
                message: "FFmpeg could not recover the checkpoint audio".to_string(),
            })
        }
        Err(error) => {
            error!("Failed to run FFmpeg: {}", error);
            Ok(AudioRecoveryStatus {
                status: "failed".to_string(),
                chunk_count,
                estimated_duration_seconds: estimated_duration,
                audio_file_path: None,
                message: "FFmpeg could not be started for audio recovery".to_string(),
            })
        }
    }
}

#[tauri::command]
pub async fn cleanup_checkpoints<R: Runtime>(
    app: AppHandle<R>,
    meeting_folder: String,
) -> Result<(), String> {
    let (_, checkpoints_dir) = validated_checkpoint_directory(&app, &meeting_folder)?;
    if let Some(checkpoints_dir) = checkpoints_dir {
        std::fs::remove_dir_all(checkpoints_dir)
            .map_err(|error| format!("Failed to remove checkpoints directory: {error}"))?;
    }
    Ok(())
}

#[tauri::command]
pub async fn has_audio_checkpoints<R: Runtime>(
    app: AppHandle<R>,
    meeting_folder: String,
) -> Result<bool, String> {
    let (_, Some(checkpoints_dir)) = validated_checkpoint_directory(&app, &meeting_folder)? else {
        return Ok(false);
    };
    for entry in std::fs::read_dir(&checkpoints_dir)
        .map_err(|error| format!("Failed to read checkpoints directory: {error}"))?
    {
        let entry =
            entry.map_err(|error| format!("Failed to inspect checkpoint entry: {error}"))?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(is_checkpoint_file_name)
        {
            validate_checkpoint_file(&entry.path(), &checkpoints_dir)?;
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::super::recording_state::DeviceType;
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn checkpoint_names_and_concat_paths_reject_injection() {
        assert!(is_checkpoint_file_name("audio_chunk_000.mp4"));
        assert!(!is_checkpoint_file_name("anything.mp4"));
        assert!(ffconcat_safe_path(Path::new("/safe/audio_chunk_000.mp4")).is_ok());
        assert!(ffconcat_safe_path(Path::new("/unsafe/quote'/audio_chunk_000.mp4")).is_err());
        assert!(ffconcat_safe_path(Path::new("/unsafe/new\nline/audio_chunk_000.mp4")).is_err());
        assert!(ffconcat_safe_path(Path::new("/unsafe/back\\slash/audio_chunk_000.mp4")).is_err());
    }

    #[tokio::test]
    async fn test_checkpoint_creation() {
        // Create temp meeting folder
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Test_Meeting");
        std::fs::create_dir_all(&meeting_folder).unwrap();
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();

        let mut saver = IncrementalAudioSaver::new(meeting_folder.clone(), 48000).unwrap();

        // Add 60 seconds worth of audio (should create 2 checkpoints)
        for i in 0..120 {
            // 120 chunks of 0.5s each
            let chunk = AudioChunk {
                data: vec![0.5f32; 24000], // 0.5s at 48kHz
                sample_rate: 48000,
                timestamp: i as f64 * 0.5, // timestamp in seconds
                chunk_id: i as u64,
                device_type: DeviceType::Microphone,
            };
            saver.add_chunk(chunk).unwrap();
        }

        // Verify 2 checkpoints created
        assert_eq!(saver.checkpoint_count, 2);

        // Finalize and verify merge
        let final_path = saver.finalize().await.unwrap();
        assert!(final_path.exists());

        // Verify checkpoints directory deleted
        assert!(!meeting_folder.join(".checkpoints").exists());
    }

    #[tokio::test]
    async fn test_empty_recording() {
        let temp_dir = tempdir().unwrap();
        let meeting_folder = temp_dir.path().join("Empty_Test");
        std::fs::create_dir_all(&meeting_folder).unwrap();
        std::fs::create_dir_all(meeting_folder.join(".checkpoints")).unwrap();

        let mut saver = IncrementalAudioSaver::new(meeting_folder.clone(), 48000).unwrap();

        // Try to finalize without adding any chunks
        let result = saver.finalize().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No audio checkpoints"));
    }
}
