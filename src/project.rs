use crate::model::Project;
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn save(path: impl AsRef<Path>, project: &Project) -> Result<()> {
    let path = path.as_ref();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let asset_dir_name = format!(
        "{}_assets",
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("beatcraft")
    );
    let asset_dir = parent.join(&asset_dir_name);
    let mut portable = project.clone();
    for track in &mut portable.tracks {
        if let Some(sample) = track.sample.as_mut() {
            let source = PathBuf::from(&sample.path);
            anyhow::ensure!(
                source.is_file(),
                "Missing sample for {}: {}",
                track.name,
                source.display()
            );
            fs::create_dir_all(&asset_dir)?;
            let original = source
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("sample.wav");
            let name = safe_name(original);
            let destination = asset_dir.join(format!("{}_{}", safe_name(&track.id), name));
            let source_canonical = source.canonicalize().unwrap_or(source.clone());
            let dest_canonical = destination.canonicalize().unwrap_or(destination.clone());
            if source_canonical != dest_canonical {
                fs::copy(&source, &destination)
                    .with_context(|| format!("Could not copy sample {}", source.display()))?;
            }
            sample.path = format!(
                "{asset_dir_name}/{}",
                destination.file_name().unwrap().to_string_lossy()
            );
        }
    }
    let bytes = serde_json::to_vec_pretty(&portable)?;
    let temp = path.with_extension("tmp");
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)?;
    Ok(())
}

pub fn load(path: impl AsRef<Path>) -> Result<Project> {
    let path = path.as_ref();
    let bytes =
        fs::read(path).with_context(|| format!("Could not read project {}", path.display()))?;
    let mut project: Project =
        serde_json::from_slice(&bytes).context("Project file is not valid BeatCraft JSON")?;
    anyhow::ensure!(
        project.format_version <= 1,
        "Project format is newer than this BeatCraft build."
    );
    project.validate()?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    for track in &mut project.tracks {
        if let Some(sample) = track.sample.as_mut() {
            let sample_path = PathBuf::from(&sample.path);
            if sample_path.is_relative() {
                sample.path = parent.join(sample_path).to_string_lossy().into_owned();
            }
        }
    }
    Ok(project)
}

fn safe_name(value: &str) -> String {
    let result: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let result = result.trim_matches('.').to_owned();
    if result.is_empty() {
        "asset".into()
    } else {
        result.chars().take(96).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{default_project, SampleRef, TrackKind};
    #[test]
    fn project_round_trips_and_copies_local_assets() {
        let temp = std::env::temp_dir().join(format!("beatcraft-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp);
        fs::create_dir_all(&temp).unwrap();
        let sample = temp.join("source sample.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&sample, spec).unwrap();
        writer.write_sample(1000_i16).unwrap();
        writer.finalize().unwrap();
        let mut project = default_project("nyx");
        let mut track = project.tracks[0].clone();
        track.id = "sample-1".into();
        track.name = "Sample".into();
        track.kind = TrackKind::Sample;
        track.sample = Some(SampleRef::new(sample.to_string_lossy().into_owned()));
        project.tracks.push(track);
        let file = temp.join("song.bcproj.json");
        save(&file, &project).unwrap();
        let loaded = load(&file).unwrap();
        let loaded_sample = loaded
            .tracks
            .iter()
            .find(|t| t.id == "sample-1")
            .unwrap()
            .sample
            .as_ref()
            .unwrap();
        assert!(Path::new(&loaded_sample.path).is_file());
        let _ = fs::remove_dir_all(&temp);
    }
}
