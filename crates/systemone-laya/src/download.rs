//! Pinned files for each Laya profile, for `s1 setup` downloads.
//!
//! The list is laya-core's embedded manifest: the five runtime files of the
//! profile at laya-core's pinned Hugging Face revision, each with its size
//! and SHA-256. It is the same manifest the runtime verifies at load.

use systemone_core::{DownloadPlan, HostError};

use crate::settings::PROFILES;

/// Files to download for `profile` (one of [`PROFILES`]).
pub fn download_plan(profile: &str) -> Result<DownloadPlan, HostError> {
    if !PROFILES.contains(&profile) {
        return Err(HostError::validation(format!(
            "unknown laya profile {profile:?}; expected one of {PROFILES:?}"
        )));
    }
    plan(profile)
}

#[cfg(feature = "laya")]
fn plan(profile: &str) -> Result<DownloadPlan, HostError> {
    let parsed: laya_core::Profile = profile.parse().map_err(crate::convert::map_error)?;
    let manifest =
        laya_core::ProfileManifest::embedded(parsed).map_err(crate::convert::map_error)?;
    Ok(DownloadPlan {
        label: format!("laya {profile}"),
        files: manifest
            .files
            .iter()
            .map(|file| systemone_core::PinnedFile {
                path: file.path.clone(),
                url: file.hub_url(),
                bytes: file.bytes,
                sha256: file.sha256.clone(),
            })
            .collect(),
    })
}

#[cfg(not(feature = "laya"))]
fn plan(_profile: &str) -> Result<DownloadPlan, HostError> {
    Err(HostError::unsupported(
        "laya downloads need a build with the laya-cpu or laya-metal feature",
    ))
}

#[cfg(all(test, feature = "laya"))]
mod tests {
    use super::*;

    #[test]
    fn every_profile_has_a_valid_plan() {
        for profile in PROFILES {
            let plan = download_plan(profile).unwrap();
            plan.validate().unwrap();
            assert_eq!(plan.files.len(), 5, "{profile}");
            assert!(
                plan.files
                    .iter()
                    .any(|file| file.path == "model.safetensors")
            );
        }
        assert!(download_plan("klingon").is_err());
    }
}
