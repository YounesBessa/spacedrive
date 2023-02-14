use crate::{library::LibraryContext, prisma::volume::*};

use rspc::Type;
use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use std::process::Command;
use swift_rs::{swift_object, Bool, SRString, UInt64};
use sysinfo::{DiskExt, RefreshKind, System, SystemExt};
use thiserror::Error;

#[cfg(target_os = "macos")]
extern "C" {
	fn native_get_mounts() -> SRString;
}

#[swift_object]
#[derive(Deserialize)]
struct VolumeFromSwift {
	name: String,
	is_root_filesystem: Bool,
	mount_point: String,
	total_capacity: UInt64,
	available_capacity: UInt64,
	is_removable: Bool,
}

#[serde_as]
#[derive(Serialize, Deserialize, Debug, Default, Clone, Type)]
pub struct Volume {
	pub name: String,
	pub mount_point: String,
	#[specta(type = String)]
	#[serde_as(as = "DisplayFromStr")]
	pub total_capacity: u64,
	#[specta(type = String)]
	#[serde_as(as = "DisplayFromStr")]
	pub available_capacity: u64,
	pub is_removable: bool,
	pub disk_type: Option<String>,
	pub file_system: Option<String>,
	pub is_root_filesystem: bool,
}

#[derive(Error, Debug)]
pub enum VolumeError {
	#[error("Database error: {0}")]
	DatabaseErr(#[from] prisma_client_rust::QueryError),
	#[error("FromUtf8Error: {0}")]
	FromUtf8Error(#[from] std::string::FromUtf8Error),
}

impl From<VolumeError> for rspc::Error {
	fn from(e: VolumeError) -> Self {
		rspc::Error::with_cause(rspc::ErrorCode::InternalServerError, e.to_string(), e)
	}
}

pub async fn save_volume(ctx: &LibraryContext) -> Result<(), VolumeError> {
	let volumes = get_volumes()?;

	// enter all volumes associate with this client add to db
	for volume in volumes {
		ctx.db
			.volume()
			.upsert(
				node_id_mount_point_name(
					ctx.node_local_id,
					volume.mount_point.to_string(),
					volume.name.to_string(),
				),
				(
					ctx.node_local_id,
					volume.name,
					volume.mount_point,
					vec![
						disk_type::set(volume.disk_type.clone()),
						filesystem::set(volume.file_system.clone()),
						total_bytes_capacity::set(volume.total_capacity.to_string()),
						total_bytes_available::set(volume.available_capacity.to_string()),
					],
				),
				vec![
					disk_type::set(volume.disk_type),
					filesystem::set(volume.file_system),
					total_bytes_capacity::set(volume.total_capacity.to_string()),
					total_bytes_available::set(volume.available_capacity.to_string()),
				],
			)
			.exec()
			.await?;
	}
	// cleanup: remove all unmodified volumes associate with this client

	Ok(())
}

// TODO: Error handling in this function
pub fn get_volumes() -> Result<Vec<Volume>, VolumeError> {
	println!("I LOVE LISTING VOLUMES");

	let system_disks_binding = System::new_with_specifics(RefreshKind::new().with_disks_list());
	let system_disks = system_disks_binding.disks();

	let mut volumes: Vec<Volume> = vec![];

	#[cfg(target_os = "macos")]
	{
		println!("Hello Macintosh! Let's get mounts...");

		// we take this data from Swift because it provides a cleaner list we don't have to hack around
		let native_mounts = unsafe {
			let native_mounts_raw = native_get_mounts();
			serde_json::from_str::<Vec<VolumeFromSwift>>(&native_mounts_raw).unwrap()
		};

		println!("OK got the mounts. Loopy time");

		native_mounts //
			.into_iter()
			.for_each(|mount| {
				println!(
					"Evaluating worthiness of mount '{}' (potential heir to the throne)",
					mount.name.to_string()
				);

				let this_system_disk = system_disks.iter().find(|disk| {
					println!(
						"\nComparing sysinfo disk mount '{}' to Swift FileManager volume mount '{}'",
						disk.mount_point().to_str().unwrap_or(""),
						mount.mount_point.to_string()
					);

					disk.mount_point().to_str().unwrap_or("") == mount.mount_point.to_string()
				});

				if this_system_disk.is_none() {
					println!(
						"No matching system disk found for mount at {}. Skipping acknowledgment.",
						mount.mount_point
					);
					return;
				};

				/*
				somewhere after here we get the following BUT ONLY ON CERTAIN SUBSEQUENT RUNS???
					Crashed Thread:        23  tokio-runtime-worker

					Exception Type:        EXC_BAD_ACCESS (SIGSEGV)
					Exception Codes:       KERN_INVALID_ADDRESS at 0x0000000000445370
					Exception Codes:       0x0000000000000001, 0x0000000000445370

					Termination Reason:    Namespace SIGNAL, Code 11 Segmentation fault: 11
					Terminating Process:   exc handler [1256]

					VM Region Info: 0x445370 is not in any region.  Bytes before following region: 4364364944
				*/

				let disk_type = match this_system_disk.unwrap().type_() {
					sysinfo::DiskType::SSD => "SSD".to_string(),
					sysinfo::DiskType::HDD => "HDD".to_string(),
					_ => "Removable Disk".to_string(),
				};

				volumes.insert(
					volumes.len(),
					Volume {
						name: mount.name,
						is_root_filesystem: mount.is_root_filesystem,
						mount_point: mount.mount_point,
						total_capacity: mount.total_capacity,
						available_capacity: mount.available_capacity,
						is_removable: mount.is_removable,
						// todo: fill there from Rust System
						disk_type: Some(disk_type),
						file_system: None,
					},
				);
			});
	}

	system_disks.iter().for_each(|disk| {
		let mut total_capacity = disk.total_space();
		let mount_point = disk.mount_point().to_str().unwrap_or("/").to_string();
		let available_capacity = disk.available_space();
		let name = disk.name().to_str().unwrap_or("Volume").to_string();
		let is_removable = disk.is_removable();

		let file_system =
			String::from_utf8(disk.file_system().to_vec()).unwrap_or_else(|_| "Err".to_string());

		let disk_type = match disk.type_() {
			sysinfo::DiskType::SSD => "SSD".to_string(),
			sysinfo::DiskType::HDD => "HDD".to_string(),
			_ => "Removable Disk".to_string(),
		};

		if total_capacity < available_capacity && cfg!(target_os = "windows") {
			let mut caption = mount_point.clone();
			caption.pop();
			let wmic_process = Command::new("cmd")
				.args([
					"/C",
					&format!("wmic logical disk where Caption='{caption}' get Size"),
				])
				.output()
				.expect("failed to execute process");
			let wmic_process_output =
				String::from_utf8(wmic_process.stdout).unwrap_or("".to_string());
			let parsed_size =
				wmic_process_output.split("\r\r\n").collect::<Vec<&str>>()[1].to_string();

			if let Ok(n) = parsed_size.trim().parse::<u64>() {
				total_capacity = n;
			}
		}

		volumes.push(Volume {
			name,
			is_root_filesystem: mount_point == "/",
			mount_point,
			total_capacity,
			available_capacity,
			is_removable,
			disk_type: Some(disk_type),
			file_system: Some(file_system),
		});
	});

	Ok(volumes)
}

// #[test]
// fn test_get_volumes() {
//   let volumes = get_volumes()?;
//   dbg!(&volumes);
//   assert!(volumes.len() > 0);
// }

// Adapted from: https://github.com/kimlimjustin/xplorer/blob/f4f3590d06783d64949766cc2975205a3b689a56/src-tauri/src/drives.rs
