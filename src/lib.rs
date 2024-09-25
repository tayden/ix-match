use anyhow::{Context, Result};
use chrono::prelude::*;
use polars::df;
use polars::prelude::*;

use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::time::Duration;

mod filesystem;
pub use filesystem::find_dir_by_pattern;



fn make_iiq_df(iiq_files: &[PathBuf]) -> Result<DataFrame> {
    let paths: Vec<String> = iiq_files
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();

    let stems: Vec<String> = iiq_files
        .iter()
        .map(|p| {
            p.file_stem()
                .context("Failed to get file stem")
                .and_then(|stem| stem.to_str().context("Failed to convert file stem to string"))
                .map(|s| s.to_owned())
        })
        .collect::<Result<Vec<String>>>()?;

    let datetimes: Vec<NaiveDateTime> = stems
        .iter()
        .map(|stem| NaiveDateTime::parse_from_str(&stem[..16], "%y%m%d_%H%M%S%3f")
            .with_context(|| format!("Failed to parse datetime from stem: {}", stem)))
        .collect::<Result<Vec<NaiveDateTime>>>()?;

    let sizes: Vec<u64> = iiq_files
        .iter()
        .map(|p| fs::metadata(p)
            .with_context(|| format!("Failed to get metadata for file: {:?}", p))
            .map(|meta| meta.len()))
        .collect::<Result<Vec<u64>>>()?;

    df!(
        "Path" => paths,
        "Stem" => stems,
        "Datetime" => datetimes,
        "Bytes" => sizes
    ).context("Failed to create DataFrame")
}

fn join_dataframes(rgb_df: &DataFrame, nir_df: &DataFrame) -> Result<DataFrame> {
    // Sort by datetime, Rename the columns to avoid conflicts, add a dummy column to match on
    let rgb_df = rgb_df.clone().lazy()
        .sort(["Datetime"], SortMultipleOptions::default())
        .select(&[
            col("Datetime").alias("Datetime_rgb"),
            col("Path").alias("Path_rgb"),
            col("Stem").alias("Stem_rgb"),
            col("Bytes").alias("Bytes_rgb"),
        ])
        .with_column(lit(1).alias("dummy"))
        .collect()?;

    let nir_df = nir_df.clone().lazy()
    .sort(["Datetime"], SortMultipleOptions::default())
    .select(&[
        col("Datetime").alias("Datetime_nir"),
        col("Path").alias("Path_nir"),
        col("Stem").alias("Stem_nir"),
        col("Bytes").alias("Bytes_nir"),
    ])
    .with_column(lit(1).alias("dummy"))
    .collect()?;

    let matched_df_rgb = rgb_df
        .join_asof_by(
            &nir_df,
            "Datetime_rgb",
            "Datetime_nir",
            ["dummy"],
            ["dummy"],
            AsofStrategy::Nearest,
            None,
        )?
        .lazy()
        .select(&[
            col("Path_rgb"),
            col("Stem_rgb"),
            col("Datetime_rgb"),
            col("Bytes_rgb"),
            col("Path_nir"),
            col("Stem_nir"),
            col("Datetime_nir"),
            col("Bytes_nir"),
        ])
        .collect()?;

    let matched_df_nir = nir_df
        .join_asof_by(
            &rgb_df,
            "Datetime_nir",
            "Datetime_rgb",
            ["dummy"],
            ["dummy"],
            AsofStrategy::Nearest,
            None,
        )?
        .lazy()
        .select(&[
            col("Path_rgb"),
            col("Stem_rgb"),
            col("Datetime_rgb"),
            col("Bytes_rgb"),
            col("Path_nir"),
            col("Stem_nir"),
            col("Datetime_nir"),
            col("Bytes_nir"),
        ])
        .collect()?;

    // Merge the two matched dataframes to imitate an outer join
    let matched_df =
        matched_df_rgb
            .vstack(&matched_df_nir)?
            .unique_stable(None, UniqueKeepStrategy::Any, None)?;

    let mut matched_df = matched_df;

    // Add a new column with the time difference
    let datetime_left = matched_df
        .column("Datetime_rgb")?
        .cast(&DataType::Datetime(TimeUnit::Microseconds, None))?;
    let datetime_right = matched_df
        .column("Datetime_nir")?
        .cast(&DataType::Datetime(TimeUnit::Microseconds, None))?;

    let time_diff = (datetime_left - datetime_right)?
        .rename(PlSmallStr::from_str("dt"))
        .deref()
        .to_owned();
    let abs_time_diff = abs(&time_diff)?;

    matched_df.with_column(abs_time_diff)?;

    Ok(matched_df)
}

pub fn get_df_column_as_paths(df: &DataFrame, column_name: &str) -> Result<Vec<PathBuf>> {
    let path_series = df.column(column_name)?.str()?;
    
    Ok(path_series
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .collect())
}

pub fn process_images(
    rgb_dir: &Path,
    nir_dir: &Path,
    match_threshold: Duration,
    keep_empty_files: bool,
    dry_run: bool,
    verbose: bool,
) -> Result<(usize, usize, usize, usize, usize)> {
    // Check that the directories exist
    let rgb_exists = rgb_dir.exists();
    let nir_exists = nir_dir.exists();
    if !rgb_exists || !nir_exists {
        return Err(anyhow::anyhow!("RGB and NIR directories do not exist"));
    } else if !rgb_exists {
        return Err(anyhow::anyhow!("RGB directory does not exist"));
    } else if !nir_exists {
        return Err(anyhow::anyhow!("NIR directory does not exist"));
    }

    // Find IIQ files
    let rgb_iiq_files = filesystem::find_files(rgb_dir, ".iiq")?;
    let nir_iiq_files = filesystem::find_files(nir_dir, ".iiq")?;

    // Create dataframes
    let mut rgb_df = make_iiq_df(&rgb_iiq_files)?;
    let mut nir_df = make_iiq_df(&nir_iiq_files)?;

    // Find 0 byte files
    let rgb_df_empty = rgb_df.clone().lazy().filter(col("Bytes").lt_eq(0)).collect()?;
    let nir_df_empty = nir_df.clone().lazy().filter(col("Bytes").lt_eq(0)).collect()?;
    
    if !keep_empty_files {
        rgb_df = rgb_df.lazy().filter(col("Bytes").gt(0)).collect()?;
        nir_df = nir_df.lazy().filter(col("Bytes").gt(0)).collect()?;
    }

    // Do the join
    let joint_df = join_dataframes(&rgb_df, &nir_df)?;

    // Split df into matched and unmatched based on threshold
    let thresh = match_threshold.as_nanos() as i64;
    let thresh_exp = lit(thresh).cast(DataType::Duration(TimeUnit::Nanoseconds));

    let matched_df = joint_df
        .clone()
        .lazy()
        .filter(col("dt").lt_eq(thresh_exp.clone()))
        .collect()?;

    let unmatched_rgb_df = joint_df
        .clone()
        .lazy()
        .join(
            matched_df.clone().lazy(),
            [col("Path_rgb")],
            [col("Path_rgb")],
            JoinArgs::new(JoinType::Anti),
        )
        .select(&[col("Stem_rgb"), col("Path_rgb")])
        .unique(None, UniqueKeepStrategy::Any)
        .collect()?;

    let unmatched_nir_df = joint_df
        .clone()
        .lazy()
        .join(
            matched_df.clone().lazy(),
            [col("Path_nir")],
            [col("Path_nir")],
            JoinArgs::new(JoinType::Anti),
        )
        .select([col("Stem_nir"), col("Path_nir")])
        .unique(None, UniqueKeepStrategy::Any)
        .collect()?;

    if verbose {
        println!("joint_df: {:?}", joint_df);
        println!("matched_df: {:?}", matched_df);
        println!("unmatched_rgb_df: {:?}", unmatched_rgb_df);
        println!("unmatched_nir_df: {:?}", unmatched_nir_df);
    }

    if !dry_run {
        // Move all matched iiq files to camera dirs root
        let matched_rgb_paths = get_df_column_as_paths(&matched_df, "Path_rgb")?;
        filesystem::move_files(matched_rgb_paths, rgb_dir, verbose)?;
        let matched_nir_paths = get_df_column_as_paths(&matched_df, "Path_nir")?;
        filesystem::move_files(matched_nir_paths, nir_dir, verbose)?;

        // Move unmatched files
        if unmatched_rgb_df.height() > 0 {
            let unmatched_rgb_dir = rgb_dir.join("unmatched");
            if verbose {
                println!("Moving unmatched RGB files to {:?}", unmatched_rgb_dir);
            }
            fs::create_dir_all(&unmatched_rgb_dir)?;
            let unmatched_rgb_paths = get_df_column_as_paths(&unmatched_rgb_df, "Path_rgb")?;
            filesystem::move_files(unmatched_rgb_paths, &unmatched_rgb_dir, verbose)?;
        }
        if unmatched_nir_df.height() > 0 {
            let unmatched_nir_dir = nir_dir.join("unmatched");
            if verbose {
                println!("Moving unmatched NIR files to {:?}", unmatched_nir_dir);
            }
            fs::create_dir_all(&unmatched_nir_dir)?;
            let unmatched_nir_paths = get_df_column_as_paths(&unmatched_nir_df, "Path_nir")?;
            filesystem::move_files(unmatched_nir_paths, &unmatched_nir_dir, verbose)?;
        }

        // Move empty files
        if !keep_empty_files {
            if rgb_df_empty.height() > 0 {
                let empty_rgb_dir = rgb_dir.join("empty");
                if verbose {
                    println!("Moving empty RGB files to {:?}", empty_rgb_dir);
                }
                fs::create_dir_all(&empty_rgb_dir)?;
                let empty_rgb_paths = get_df_column_as_paths(&rgb_df_empty, "Path")?;
                filesystem::move_files(empty_rgb_paths, &empty_rgb_dir, verbose)?;
            }
            if nir_df_empty.height() > 0 {
                let empty_nir_dir = nir_dir.join("empty");
                if verbose {
                    println!("Moving empty NIR files to {:?}", empty_nir_dir);
                }
                fs::create_dir_all(&empty_nir_dir)?;
                let empty_nir_paths = get_df_column_as_paths(&nir_df_empty, "Path")?;
                filesystem::move_files(empty_nir_paths, &empty_nir_dir, verbose)?;
            }
        }
    }

    Ok((rgb_iiq_files.len(), nir_iiq_files.len(), matched_df.height(), rgb_df_empty.height(), nir_df_empty.height()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;
    use tempfile::TempDir;

    use std::fs;
    use std::time::Duration;

    #[test]
    fn test_make_iiq_df() {
        let temp_dir = TempDir::new().unwrap();
        let base_path = temp_dir.path();

        let files = vec![
            base_path.join("210101_120000000.iiq"),
            base_path.join("210101_120001000.iiq"),
        ];

        files.iter().for_each(|file| {
            fs::write(file, "content").unwrap();
        });

        let df = make_iiq_df(&files).unwrap();

        assert_eq!(df.shape(), (2, 4));
        assert_eq!(df.column("Path").unwrap().len(), 2);
        assert_eq!(df.column("Stem").unwrap().len(), 2);
        assert_eq!(df.column("Datetime").unwrap().len(), 2);
        assert_eq!(df.column("Bytes").unwrap().len(), 2);

        let stems: Vec<&str> = df
            .column("Stem")
            .unwrap()
            .str()
            .unwrap()
            .into_iter()
            .collect::<Vec<Option<&str>>>()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(stems, vec!["210101_120000000", "210101_120001000"]);
    }

    #[test]
    fn test_get_df_column_as_paths() {
        let df = df!(
            "Path" => &["/path/to/file1.iiq", "/path/to/file2.iiq"],
            "Stem" => &["file1", "file2"],
            "Datetime" => &[
                NaiveDateTime::parse_from_str("2021-01-01 12:00:00", "%Y-%m-%d %H:%M:%S").unwrap(),
                NaiveDateTime::parse_from_str("2021-01-01 12:00:01", "%Y-%m-%d %H:%M:%S").unwrap(),
            ],
            "Bytes" => &[100, 200]
        ).unwrap();

        let paths = get_df_column_as_paths(&df, "Path").unwrap();
        assert_eq!(paths, vec![PathBuf::from("/path/to/file1.iiq"), PathBuf::from("/path/to/file2.iiq")]);
    }

    #[test]
    fn test_join_dataframes() {
        let rgb_data = df!(
            "Datetime" => &[
                NaiveDateTime::parse_from_str("2021-01-01 12:00:00", "%Y-%m-%d %H:%M:%S").unwrap(),
                NaiveDateTime::parse_from_str("2021-01-01 12:00:01", "%Y-%m-%d %H:%M:%S").unwrap(),
            ],
            "Path" => &["/path/to/rgb1.iiq", "/path/to/rgb2.iiq"],
            "Stem" => &["rgb1", "rgb2"],
            "Bytes" => &[100, 200]
        )
        .unwrap();

        let nir_data = df!(
            "Datetime" => &[
                NaiveDateTime::parse_from_str("2021-01-01 12:00:00", "%Y-%m-%d %H:%M:%S").unwrap(),
                NaiveDateTime::parse_from_str("2021-01-01 12:00:02", "%Y-%m-%d %H:%M:%S").unwrap(),
            ],
            "Path" => &["/path/to/nir1.iiq", "/path/to/nir2.iiq"],
            "Stem" => &["nir1", "nir2"],
            "Bytes" => &[150, 250]
        )
        .unwrap();

        let result = join_dataframes(&rgb_data, &nir_data).unwrap();

        assert_eq!(result.shape(), (2, 9));
        assert_eq!(result.column("dt").unwrap().null_count(), 0);
    }

    #[test]
    fn test_process_images() {
        let temp_dir = TempDir::new().unwrap();
        let rgb_dir = temp_dir.path().join("rgb");
        let nir_dir = temp_dir.path().join("nir");
        fs::create_dir_all(&rgb_dir).unwrap();
        fs::create_dir_all(&nir_dir).unwrap();

        // Create test files
        fs::write(rgb_dir.join("210101_120000000.iiq"), "content").unwrap();
        fs::write(nir_dir.join("210101_120000100.iiq"), "content").unwrap();
        fs::write(rgb_dir.join("210101_120001000.iiq"), "content").unwrap();
        fs::write(nir_dir.join("210101_120001100.iiq"), "content").unwrap();

        let threshold = Duration::from_millis(200);
        let (rgb_count, nir_count, matched_count, empty_rgb_count, empty_nir_count) =
            process_images(&rgb_dir, &nir_dir, threshold, false, false, false).unwrap();

        assert_eq!(rgb_count, 2);
        assert_eq!(nir_count, 2);
        assert_eq!(matched_count, 2);
        assert_eq!(empty_rgb_count, 0);
        assert_eq!(empty_nir_count, 0);

        // Check if files are in their original locations
        // (process_images doesn't move matched files in this case)
        assert!(rgb_dir.join("210101_120000000.iiq").exists());
        assert!(rgb_dir.join("210101_120001000.iiq").exists());
        assert!(nir_dir.join("210101_120000100.iiq").exists());
        assert!(nir_dir.join("210101_120001100.iiq").exists());

        // Unmatched directories should not be created in this case
        assert!(!rgb_dir.join("unmatched").exists());
        assert!(!nir_dir.join("unmatched").exists());
    }

    #[test]
    fn test_process_images_dry_run() {
        let temp_dir = TempDir::new().unwrap();
        let rgb_dir = temp_dir.path().join("rgb");
        let nir_dir = temp_dir.path().join("nir");
        fs::create_dir_all(&rgb_dir).unwrap();
        fs::create_dir_all(&nir_dir).unwrap();

        // Create test files
        fs::write(rgb_dir.join("210101_120000000.iiq"), "content").unwrap();
        fs::write(rgb_dir.join("210101_120001000.iiq"), "content").unwrap();
        fs::write(nir_dir.join("210101_120000100.iiq"), "content").unwrap();
        fs::write(nir_dir.join("210101_120005000.iiq"), "content").unwrap(); // This one won't match

        let threshold = Duration::from_millis(200);
        let (rgb_count, nir_count, matched_count, empty_rgb_count, empty_nir_count) =
            process_images(&rgb_dir, &nir_dir, threshold,true, true, false).unwrap();

        assert_eq!(rgb_count, 2);
        assert_eq!(nir_count, 2);
        assert_eq!(matched_count, 1);
        assert_eq!(empty_rgb_count, 0);
        assert_eq!(empty_nir_count, 0);

        // Check if all files are in their original locations (dry run)
        assert!(rgb_dir.join("210101_120000000.iiq").exists());
        assert!(rgb_dir.join("210101_120001000.iiq").exists());
        assert!(nir_dir.join("210101_120000100.iiq").exists());
        assert!(nir_dir.join("210101_120005000.iiq").exists());
        assert!(!rgb_dir.join("unmatched").exists());
        assert!(!nir_dir.join("unmatched").exists());
    }

    #[test]
    fn test_process_images_with_unmatched() {
        let temp_dir = TempDir::new().unwrap();
        let rgb_dir = temp_dir.path().join("rgb");
        let nir_dir = temp_dir.path().join("nir");
        fs::create_dir_all(&rgb_dir).unwrap();
        fs::create_dir_all(&nir_dir).unwrap();

        // Create test files
        fs::write(rgb_dir.join("210101_120000000.iiq"), "content").unwrap();
        fs::write(nir_dir.join("210101_120000100.iiq"), "content").unwrap();
        // These won't match
        fs::write(rgb_dir.join("210101_120001000.iiq"), "content").unwrap();
        fs::write(nir_dir.join("210101_120005000.iiq"), "content").unwrap();

        let threshold = Duration::from_millis(200);
        let (rgb_count, nir_count, matched_count, empty_rgb_count, empty_nir_count) =
            process_images(&rgb_dir, &nir_dir, threshold, true, false, false).unwrap();

        assert_eq!(rgb_count, 2);
        assert_eq!(nir_count, 2);
        assert_eq!(matched_count, 1);
        assert_eq!(empty_rgb_count, 0);
        assert_eq!(empty_nir_count, 0);

        // Check if matched files are in their original locations
        assert!(rgb_dir.join("210101_120000000.iiq").exists());
        assert!(nir_dir.join("210101_120000100.iiq").exists());

        // Check if unmatched files are moved to the unmatched directory
        assert!(rgb_dir
            .join("unmatched")
            .join("210101_120001000.iiq")
            .exists());
        assert!(!rgb_dir.join("210101_120001000.iiq").exists());
        assert!(nir_dir
            .join("unmatched")
            .join("210101_120005000.iiq")
            .exists());
        assert!(!nir_dir.join("210101_120005000.iiq").exists());
    }

    #[test]
    fn test_process_images_with_uneven_numbers() {
        let temp_dir = TempDir::new().unwrap();
        let rgb_dir = temp_dir.path().join("rgb");
        let nir_dir = temp_dir.path().join("nir");
        fs::create_dir_all(&rgb_dir).unwrap();
        fs::create_dir_all(&nir_dir).unwrap();

        // Create test files
        fs::write(rgb_dir.join("210101_120000000.iiq"), "content").unwrap();
        fs::write(nir_dir.join("210101_120000100.iiq"), "content").unwrap();
        // These won't match
        fs::write(nir_dir.join("210101_120005000.iiq"), "content").unwrap();

        let threshold = Duration::from_millis(200);
        let (rgb_count, nir_count, matched_count, empty_rgb_count, empty_nir_count) =
            process_images(&rgb_dir, &nir_dir, threshold, true, false, false).unwrap();

        assert_eq!(rgb_count, 1);
        assert_eq!(nir_count, 2);
        assert_eq!(matched_count, 1);
        assert_eq!(empty_rgb_count, 0);
        assert_eq!(empty_nir_count, 0);

        // Check if matched files are in their original locations
        assert!(rgb_dir.join("210101_120000000.iiq").exists());
        assert!(nir_dir.join("210101_120000100.iiq").exists());

        // Check if unmatched files are moved to the unmatched directory
        assert!(!rgb_dir.join("unmatched").exists());
        assert!(nir_dir
            .join("unmatched")
            .join("210101_120005000.iiq")
            .exists());
        assert!(!nir_dir.join("210101_120005000.iiq").exists());
    }

    #[test]
    fn test_process_images_with_no_dirs() {
        let temp_dir = TempDir::new().unwrap();
        let rgb_dir = temp_dir.path().join("rgb");
        let nir_dir = temp_dir.path().join("nir");

        let threshold = Duration::from_millis(200);
        let result = process_images(&rgb_dir, &nir_dir, threshold, true, false, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_process_images_with_keep_empty() {
        let temp_dir = TempDir::new().unwrap();
        let rgb_dir = temp_dir.path().join("rgb");
        let nir_dir = temp_dir.path().join("nir");
        fs::create_dir_all(&rgb_dir).unwrap();
        fs::create_dir_all(&nir_dir).unwrap();

        // Create test files
        fs::write(rgb_dir.join("210101_120000000.iiq"), "content").unwrap();
        fs::write(rgb_dir.join("210101_130000000.iiq"), "").unwrap();
        fs::write(nir_dir.join("210101_120000100.iiq"), "content").unwrap();
        fs::write(nir_dir.join("210101_130000100.iiq"), "").unwrap();

        let threshold = Duration::from_millis(200);
        let (rgb_count, nir_count, matched_count, empty_rgb_count, empty_nir_count) =
            process_images(&rgb_dir, &nir_dir, threshold, true, false, false).unwrap();

        assert_eq!(rgb_count, 2);
        assert_eq!(nir_count, 2);
        assert_eq!(matched_count, 2);
        assert_eq!(empty_rgb_count, 1);
        assert_eq!(empty_nir_count, 1);

        // Check if matched files are in their original locations
        assert!(rgb_dir.join("210101_120000000.iiq").exists());
        assert!(nir_dir.join("210101_120000100.iiq").exists());
        assert!(rgb_dir.join("210101_130000000.iiq").exists());
        assert!(nir_dir.join("210101_130000100.iiq").exists());

        // Check that no empty directories were created
        assert!(!rgb_dir.join("empty").exists());
        assert!(!nir_dir.join("empty").exists());
    }

    #[test]
    fn test_process_images_with_no_keep_empty() {
        let temp_dir = TempDir::new().unwrap();
        let rgb_dir = temp_dir.path().join("rgb");
        let nir_dir = temp_dir.path().join("nir");
        fs::create_dir_all(&rgb_dir).unwrap();
        fs::create_dir_all(&nir_dir).unwrap();

        // Create test files
        fs::write(rgb_dir.join("210101_120000000.iiq"), "content").unwrap();
        fs::write(rgb_dir.join("210101_130000000.iiq"), "").unwrap();
        fs::write(nir_dir.join("210101_120000100.iiq"), "content").unwrap();
        fs::write(nir_dir.join("210101_130000100.iiq"), "").unwrap();

        let threshold = Duration::from_millis(200);
        let (rgb_count, nir_count, matched_count, empty_rgb_count, empty_nir_count) =
            process_images(&rgb_dir, &nir_dir, threshold, false, false, false).unwrap();

        assert_eq!(rgb_count, 2);
        assert_eq!(nir_count, 2);
        assert_eq!(matched_count, 1);
        assert_eq!(empty_rgb_count, 1);
        assert_eq!(empty_nir_count, 1);

        // Check if matched files are in their original locations
        assert!(rgb_dir.join("210101_120000000.iiq").exists());
        assert!(nir_dir.join("210101_120000100.iiq").exists());

        // Check if empty files are moved to the empty directory
        assert!(rgb_dir.join("empty").join("210101_130000000.iiq").exists());
        assert!(!rgb_dir.join("210101_130000000.iiq").exists());
        assert!(nir_dir.join("empty").join("210101_130000100.iiq").exists());
        assert!(!nir_dir.join("210101_130000100.iiq").exists());
    }
}
