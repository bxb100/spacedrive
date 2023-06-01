use crate::{
	api::CoreEvent,
	invalidate_query,
	job::{JobError, JobReportUpdate, JobResult, JobState, WorkerContext},
	library::Library,
	location::{
		file_path_helper::{file_path_for_thumbnailer, FilePathError, IsolatedFilePathData},
		LocationId,
	},
	prisma::location,
	util::error::FileIOError,
};

use std::{
	error::Error,
	ops::Deref,
	path::{Path, PathBuf},
};

use sd_file_ext::extensions::{Extension, ImageExtension};

#[cfg(feature = "ffmpeg")]
use sd_file_ext::extensions::VideoExtension;

use image::{self, imageops, DynamicImage, GenericImageView};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{fs, io, task::block_in_place};
use tracing::{error, info, trace, warn};
use webp::Encoder;

use self::thumbnailer_job::ThumbnailerJob;

mod shallow;
pub mod thumbnailer_job;

pub use shallow::*;

const THUMBNAIL_SIZE_FACTOR: f32 = 0.2;
const THUMBNAIL_QUALITY: f32 = 30.0;
pub const THUMBNAIL_CACHE_DIR_NAME: &str = "thumbnails";

/// This does not check if a thumbnail exists, it just returns the path that it would exist at
pub fn get_thumbnail_path(library: &Library, cas_id: &str) -> PathBuf {
	library
		.config()
		.data_directory()
		.join(THUMBNAIL_CACHE_DIR_NAME)
		.join(cas_id)
		.with_extension("webp")
}

#[cfg(feature = "ffmpeg")]
static FILTERED_VIDEO_EXTENSIONS: Lazy<Vec<Extension>> = Lazy::new(|| {
	sd_file_ext::extensions::ALL_VIDEO_EXTENSIONS
		.iter()
		.map(Clone::clone)
		.filter(can_generate_thumbnail_for_video)
		.map(Extension::Video)
		.collect()
});

static FILTERED_IMAGE_EXTENSIONS: Lazy<Vec<Extension>> = Lazy::new(|| {
	sd_file_ext::extensions::ALL_IMAGE_EXTENSIONS
		.iter()
		.map(Clone::clone)
		.filter(can_generate_thumbnail_for_image)
		.map(Extension::Image)
		.collect()
});

#[derive(Debug, Serialize, Deserialize)]
pub struct ThumbnailerJobState {
	thumbnail_dir: PathBuf,
	location_path: PathBuf,
	report: ThumbnailerJobReport,
}

#[derive(Error, Debug)]
pub enum ThumbnailerError {
	#[error("sub path not found: <path='{}'>", .0.display())]
	SubPathNotFound(Box<Path>),

	// Internal errors
	#[error("database error")]
	Database(#[from] prisma_client_rust::QueryError),
	#[error(transparent)]
	FilePath(#[from] FilePathError),
	#[error(transparent)]
	FileIO(#[from] FileIOError),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ThumbnailerJobReport {
	location_id: LocationId,
	path: PathBuf,
	thumbnails_created: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy)]
enum ThumbnailerJobStepKind {
	Image,
	#[cfg(feature = "ffmpeg")]
	Video,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ThumbnailerJobStep {
	file_path: file_path_for_thumbnailer::Data,
	kind: ThumbnailerJobStepKind,
}

// TOOD(brxken128): validate avci and avcs
#[cfg(all(feature = "heif", not(target_os = "linux")))]
const HEIF_EXTENSIONS: [&str; 7] = ["heif", "heifs", "heic", "heics", "avif", "avci", "avcs"];

pub async fn generate_image_thumbnail<P: AsRef<Path>>(
	file_path: P,
	output_path: P,
) -> Result<(), Box<dyn Error>> {
	// Webp creation has blocking code
	let webp = block_in_place(|| -> Result<Vec<u8>, Box<dyn Error>> {
		#[cfg(all(feature = "heif", not(target_os = "linux")))]
		let img = {
			let ext = file_path
				.as_ref()
				.extension()
				.unwrap_or_default()
				.to_ascii_lowercase();
			if HEIF_EXTENSIONS
				.iter()
				.any(|e| ext == std::ffi::OsStr::new(e))
			{
				sd_heif::heif_to_dynamic_image(file_path.as_ref())?
			} else {
				image::open(file_path)?
			}
		};

		#[cfg(not(all(feature = "heif", not(target_os = "linux"))))]
		let img = image::open(file_path)?;

		let (w, h) = img.dimensions();
		// Optionally, resize the existing photo and convert back into DynamicImage
		let img = DynamicImage::ImageRgba8(imageops::resize(
			&img,
			// FIXME : Think of a better heuristic to get the thumbnail size
			(w as f32 * THUMBNAIL_SIZE_FACTOR) as u32,
			(h as f32 * THUMBNAIL_SIZE_FACTOR) as u32,
			imageops::FilterType::Triangle,
		));
		// Create the WebP encoder for the above image
		let encoder = Encoder::from_image(&img)?;

		// Encode the image at a specified quality 0-100

		// Type WebPMemory is !Send, which makes the Future in this function !Send,
		// this make us `deref` to have a `&[u8]` and then `to_owned` to make a Vec<u8>
		// which implies on a unwanted clone...
		Ok(encoder.encode(THUMBNAIL_QUALITY).deref().to_owned())
	})?;

	fs::write(output_path, &webp).await.map_err(Into::into)
}

#[cfg(feature = "ffmpeg")]
pub async fn generate_video_thumbnail<P: AsRef<Path>>(
	file_path: P,
	output_path: P,
) -> Result<(), Box<dyn Error>> {
	use sd_ffmpeg::to_thumbnail;

	to_thumbnail(file_path, output_path, 256, THUMBNAIL_QUALITY).await?;

	Ok(())
}

#[cfg(feature = "ffmpeg")]
pub const fn can_generate_thumbnail_for_video(video_extension: &VideoExtension) -> bool {
	use VideoExtension::*;
	// File extensions that are specifically not supported by the thumbnailer
	!matches!(video_extension, Mpg | Swf | M2v | Hevc | M2ts | Mts | Ts)
}

pub const fn can_generate_thumbnail_for_image(image_extension: &ImageExtension) -> bool {
	use ImageExtension::*;

	#[cfg(all(feature = "heif", not(target_os = "linux")))]
	let res = matches!(
		image_extension,
		Jpg | Jpeg | Png | Webp | Gif | Heic | Heics | Heif | Heifs | Avif
	);

	#[cfg(not(all(feature = "heif", not(target_os = "linux"))))]
	let res = matches!(image_extension, Jpg | Jpeg | Png | Webp | Gif);

	res
}

fn finalize_thumbnailer(data: &ThumbnailerJobState, ctx: WorkerContext) -> JobResult {
	info!(
		"Finished thumbnail generation for location {} at {}",
		data.report.location_id,
		data.report.path.display()
	);

	if data.report.thumbnails_created > 0 {
		invalidate_query!(ctx.library, "search.paths");
	}

	Ok(Some(serde_json::to_value(&data.report)?))
}

async fn process_step(
	state: &mut JobState<ThumbnailerJob>,
	ctx: WorkerContext,
) -> Result<(), JobError> {
	let step = &state.steps[0];

	ctx.progress(vec![JobReportUpdate::Message(format!(
		"Processing {}",
		step.file_path.materialized_path
	))]);

	let data = state
		.data
		.as_mut()
		.expect("critical error: missing data on job state");

	let step_result = inner_process_step(
		&step,
		&data.location_path,
		&data.thumbnail_dir,
		&state.init.location,
		&ctx.library,
	)
	.await;

	data.report.thumbnails_created += 1;

	ctx.progress(vec![JobReportUpdate::CompletedTaskCount(
		state.step_number + 1,
	)]);

	step_result
}

pub async fn inner_process_step(
	step: &ThumbnailerJobStep,
	location_path: &PathBuf,
	thumbnail_dir: &PathBuf,
	location: &location::Data,
	library: &Library,
) -> Result<(), JobError> {
	let ThumbnailerJobStep { file_path, kind } = step;

	// assemble the file path
	let path = location_path.join(IsolatedFilePathData::from((location.id, file_path)));
	trace!("image_file {:?}", file_path);

	// get cas_id, if none found skip
	let Some(cas_id) = &file_path.cas_id else {
		warn!(
			"skipping thumbnail generation for {}",
			file_path.materialized_path
		);

		return Ok(());
	};

	// Define and write the WebP-encoded file to a given path
	let output_path = thumbnail_dir.join(format!("{cas_id}.webp"));

	match fs::metadata(&output_path).await {
		Ok(_) => {
			info!("Thumb exists, skipping... {}", output_path.display());
		}
		Err(e) if e.kind() == io::ErrorKind::NotFound => {
			info!("Writing {:?} to {:?}", path, output_path);

			match kind {
				ThumbnailerJobStepKind::Image => {
					if let Err(e) = generate_image_thumbnail(&path, &output_path).await {
						error!("Error generating thumb for image {:#?}", e);
					}
				}
				#[cfg(feature = "ffmpeg")]
				ThumbnailerJobStepKind::Video => {
					if let Err(e) = generate_video_thumbnail(&path, &output_path).await {
						error!("Error generating thumb for video: {:?} {:#?}", &path, e);
					}
				}
			}

			println!("emitting new thumbnail event");
			library.emit(CoreEvent::NewThumbnail {
				cas_id: cas_id.clone(),
			});
		}
		Err(e) => return Err(ThumbnailerError::from(FileIOError::from((output_path, e))).into()),
	}

	Ok(())
}
