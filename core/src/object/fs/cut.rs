use crate::{
	extract_job_data, invalidate_query,
	job::{
		JobError, JobInitData, JobReportUpdate, JobResult, JobState, StatefulJob, WorkerContext,
	},
	library::Library,
	location::file_path_helper::push_location_relative_path,
	object::fs::{construct_target_filename, error::FileSystemJobsError},
	prisma::{file_path, location},
	util::error::FileIOError,
};

use std::{hash::Hash, path::PathBuf};

use serde::{Deserialize, Serialize};
use specta::Type;
use tokio::{fs, io};
use tracing::{trace, warn};

use super::{fetch_source_and_target_location_paths, get_many_files_datas, FileData};

pub struct FileCutterJob {}

#[derive(Serialize, Deserialize, Hash, Type)]
pub struct FileCutterJobInit {
	pub source_location_id: location::id::Type,
	pub target_location_id: location::id::Type,
	pub sources_file_path_ids: Vec<file_path::id::Type>,
	pub target_location_relative_directory_path: PathBuf,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileCutterJobState {
	full_target_directory_path: PathBuf,
}

impl JobInitData for FileCutterJobInit {
	type Job = FileCutterJob;
}

#[async_trait::async_trait]
impl StatefulJob for FileCutterJob {
	type Init = FileCutterJobInit;
	type Data = FileCutterJobState;
	type Step = FileData;

	const NAME: &'static str = "file_cutter";

	fn new() -> Self {
		Self {}
	}

	async fn init(
		&self,
		ctx: &mut WorkerContext,
		state: &mut JobState<Self>,
	) -> Result<(), JobError> {
		let Library { db, .. } = &ctx.library;

		let (sources_location_path, targets_location_path) =
			fetch_source_and_target_location_paths(
				db,
				state.init.source_location_id,
				state.init.target_location_id,
			)
			.await?;

		let full_target_directory_path = push_location_relative_path(
			targets_location_path,
			&state.init.target_location_relative_directory_path,
		);

		state.data = Some(FileCutterJobState {
			full_target_directory_path,
		});

		state.steps = get_many_files_datas(
			db,
			&sources_location_path,
			&state.init.sources_file_path_ids,
		)
		.await?
		.into();

		ctx.progress(vec![JobReportUpdate::TaskCount(state.steps.len())]);

		Ok(())
	}

	async fn execute_step(
		&self,
		ctx: &mut WorkerContext,
		state: &mut JobState<Self>,
	) -> Result<(), JobError> {
		let file_data = &state.steps[0];

		let full_output = extract_job_data!(state)
			.full_target_directory_path
			.join(construct_target_filename(file_data, &None)?);

		let res = if file_data.full_path == full_output {
			// File is already here, do nothing
			Ok(())
		} else {
			match fs::metadata(&full_output).await {
				Ok(_) => {
					warn!(
						"Skipping {} as it would be overwritten",
						full_output.display()
					);

					return Err(JobError::StepCompletedWithErrors(vec![
						FileSystemJobsError::WouldOverwrite(full_output.into_boxed_path())
							.to_string(),
					]));
				}
				Err(e) if e.kind() == io::ErrorKind::NotFound => {
					trace!(
						"Cutting {} to {}",
						file_data.full_path.display(),
						full_output.display()
					);

					fs::rename(&file_data.full_path, &full_output)
						.await
						.map_err(|e| FileIOError::from((&file_data.full_path, e)))?;

					Ok(())
				}

				Err(e) => return Err(FileIOError::from((&full_output, e)).into()),
			}
		};

		ctx.progress(vec![JobReportUpdate::CompletedTaskCount(
			state.step_number + 1,
		)]);

		res
	}

	async fn finalize(&mut self, ctx: &mut WorkerContext, state: &mut JobState<Self>) -> JobResult {
		invalidate_query!(ctx.library, "search.paths");

		Ok(Some(serde_json::to_value(&state.init)?))
	}
}
