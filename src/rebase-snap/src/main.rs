// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::unix::io::AsRawFd;
use std::{env, process};

use utils::arg_parser::{ArgParser, Argument, Arguments};
use utils::seek_hole::SeekHole;

const REBASE_SNAP_VERSION: &str = env!("FIRECRACKER_VERSION");
const EXIT_CODE_SUCCESS: i32 = 0;
const BASE_FILE: &str = "base-file";
const DIFF_FILE: &str = "diff-file";

#[derive(Debug)]
enum Error {
    InvalidBaseFile(std::io::Error),
    InvalidDiffFile(std::io::Error),
    SeekData(std::io::Error),
    SeekHole(std::io::Error),
    Seek(std::io::Error),
    Sendfile(std::io::Error),
    Metadata(std::io::Error),
}

fn build_arg_parser<'a>() -> ArgParser<'a> {
    let arg_parser = ArgParser::new()
        .arg(
            Argument::new(BASE_FILE)
                .required(true)
                .takes_value(true)
                .help("File path of the base mem snapshot."),
        )
        .arg(
            Argument::new(DIFF_FILE)
                .required(true)
                .takes_value(true)
                .help("File path of the diff mem snapshot."),
        );

    arg_parser
}

fn extract_args<'a>(arg_parser: &'a mut ArgParser<'a>) -> &'a Arguments<'a> {
    arg_parser.parse_from_cmdline().unwrap_or_else(|err| {
        panic!(
            "Arguments parsing error: {} \n\nFor more information try --help.",
            err
        );
    });

    if arg_parser.arguments().flag_present("help") {
        println!("Rebase_snap v{}", REBASE_SNAP_VERSION);
        println!(
            "Tool that copies all the non-sparse sections from a diff file onto a base file\n"
        );
        println!("{}", arg_parser.formatted_help());
        process::exit(EXIT_CODE_SUCCESS);
    }
    if arg_parser.arguments().flag_present("version") {
        println!("Rebase_snap v{}\n", REBASE_SNAP_VERSION);
        process::exit(EXIT_CODE_SUCCESS);
    }

    arg_parser.arguments()
}

fn parse_args(args: &Arguments) -> Result<(File, File), Error> {
    // Safe to unwrap since the required arguments are checked as part of
    // `arg_parser.parse_from_cmdline()`
    let base_file_path = args.single_value(BASE_FILE).unwrap();
    let base_file = OpenOptions::new()
        .write(true)
        .open(base_file_path)
        .map_err(Error::InvalidBaseFile)?;
    // Safe to unwrap since the required arguments are checked as part of
    // `arg_parser.parse_from_cmdline()`
    let diff_file_path = args.single_value(DIFF_FILE).unwrap();
    let diff_file = OpenOptions::new()
        .read(true)
        .open(diff_file_path)
        .map_err(Error::InvalidDiffFile)?;

    Ok((base_file, diff_file))
}

fn rebase(base_file: &mut File, diff_file: &mut File) -> Result<(), Error> {
    let mut cursor: u64 = 0;
    while let Some(block_start) = diff_file.seek_data(cursor).map_err(Error::SeekData)? {
        cursor = block_start;
        let block_end = match diff_file.seek_hole(block_start).map_err(Error::SeekHole)? {
            Some(hole_start) => hole_start,
            None => diff_file.metadata().map_err(Error::Metadata)?.len(),
        };

        while cursor < block_end {
            base_file
                .seek(SeekFrom::Start(cursor))
                .map_err(Error::Seek)?;

            // SAFETY: Safe because the parameters are valid.
            let num_transferred_bytes = unsafe {
                libc::sendfile64(
                    base_file.as_raw_fd(),
                    diff_file.as_raw_fd(),
                    (&mut cursor as *mut u64).cast::<i64>(),
                    block_end.saturating_sub(cursor) as usize,
                )
            };
            if num_transferred_bytes < 0 {
                return Err(Error::Sendfile(std::io::Error::last_os_error()));
            }
        }
    }

    Ok(())
}

fn main() {
    let mut arg_parser = build_arg_parser();
    let args = extract_args(&mut arg_parser);
    let (mut base_file, mut diff_file) =
        parse_args(args).unwrap_or_else(|err| panic!("Error parsing the cmd line args: {:?}", err));

    rebase(&mut base_file, &mut diff_file)
        .unwrap_or_else(|err| panic!("Error merging the files: {:?}", err));
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::fs::FileExt;

    use utils::{rand, tempfile};

    use super::*;

    macro_rules! assert_err {
        ($expression:expr, $($pattern:tt)+) => {
            match $expression {
                Err($($pattern)+) => (),
                ref err =>  {
                    println!("expected `{}` but got `{:?}`", stringify!($($pattern)+), err);
                    assert!(false)
                }
            }
        }
    }

    #[test]
    fn test_parse_args() {
        let base_file = tempfile::TempFile::new().unwrap();
        let base_file_path = base_file.as_path().to_str().unwrap().to_string();
        let diff_file = tempfile::TempFile::new().unwrap();
        let diff_file_path = diff_file.as_path().to_str().unwrap().to_string();

        let arg_parser = build_arg_parser();
        let arguments = &mut arg_parser.arguments().clone();
        arguments
            .parse(
                vec![
                    "rebase_snap",
                    "--base-file",
                    "wrong_file",
                    "--diff-file",
                    "diff_file",
                ]
                .into_iter()
                .map(String::from)
                .collect::<Vec<String>>()
                .as_ref(),
            )
            .unwrap();
        assert_err!(parse_args(arguments), Error::InvalidBaseFile(_));

        let arguments = &mut arg_parser.arguments().clone();
        arguments
            .parse(
                vec![
                    "rebase_snap",
                    "--base-file",
                    &base_file_path,
                    "--diff-file",
                    "diff_file",
                ]
                .into_iter()
                .map(String::from)
                .collect::<Vec<String>>()
                .as_ref(),
            )
            .unwrap();
        assert_err!(parse_args(arguments), Error::InvalidDiffFile(_));

        let arguments = &mut arg_parser.arguments().clone();
        arguments
            .parse(
                vec![
                    "rebase_snap",
                    "--base-file",
                    &base_file_path,
                    "--diff-file",
                    &diff_file_path,
                ]
                .into_iter()
                .map(String::from)
                .collect::<Vec<String>>()
                .as_ref(),
            )
            .unwrap();
        assert!(parse_args(arguments).is_ok());
    }

    fn check_file_content(file: &mut File, expected_content: &[u8]) {
        let mut buf = vec![0u8; expected_content.len()];
        file.read_exact_at(buf.as_mut_slice(), 0).unwrap();
        assert_eq!(&buf, expected_content);
    }

    #[test]
    fn test_rebase_corner_cases() {
        let mut base_file = tempfile::TempFile::new().unwrap().into_file();
        let mut diff_file = tempfile::TempFile::new().unwrap().into_file();

        // 1. Empty files
        rebase(&mut base_file, &mut diff_file).unwrap();
        assert_eq!(base_file.metadata().unwrap().len(), 0);

        let initial_base_file_content = rand::rand_alphanumerics(50000).into_string().unwrap();
        base_file
            .write_all(initial_base_file_content.as_bytes())
            .unwrap();

        // 2. Diff file that has only holes
        diff_file
            .set_len(initial_base_file_content.len() as u64)
            .unwrap();
        rebase(&mut base_file, &mut diff_file).unwrap();
        check_file_content(&mut base_file, initial_base_file_content.as_bytes());

        // 3. Diff file that has only data
        let diff_data = rand::rand_alphanumerics(50000).into_string().unwrap();
        diff_file.write_all(diff_data.as_bytes()).unwrap();
        rebase(&mut base_file, &mut diff_file).unwrap();
        check_file_content(&mut base_file, diff_data.as_bytes());
    }

    #[test]
    fn test_rebase() {
        // The filesystem punches holes only for blocks >= 4096.
        // It doesn't make sense to test for smaller ones.
        let block_sizes: &[usize] = &[4096, 8192];
        for &block_size in block_sizes {
            let mut expected_result = vec![];
            let mut base_file = tempfile::TempFile::new().unwrap().into_file();
            let mut diff_file = tempfile::TempFile::new().unwrap().into_file();

            // 1. Populated block both in base and diff file
            let base_block = rand::rand_alphanumerics(block_size).into_string().unwrap();
            base_file.write_all(base_block.as_bytes()).unwrap();
            let diff_block = rand::rand_alphanumerics(block_size).into_string().unwrap();
            diff_file.write_all(diff_block.as_bytes()).unwrap();
            expected_result.append(&mut diff_block.into_bytes());

            // 2. Populated block in base file, hole in diff file
            let base_block = rand::rand_alphanumerics(block_size).into_string().unwrap();
            base_file.write_all(base_block.as_bytes()).unwrap();
            diff_file
                .seek(SeekFrom::Current(block_size as i64))
                .unwrap();
            expected_result.append(&mut base_block.into_bytes());

            // 3. Populated block in base file, zeroes block in diff file
            let base_block = rand::rand_alphanumerics(block_size).into_string().unwrap();
            base_file.write_all(base_block.as_bytes()).unwrap();
            let mut diff_block = vec![0u8; block_size];
            diff_file.write_all(&diff_block).unwrap();
            expected_result.append(&mut diff_block);

            // Rebase and check the result
            rebase(&mut base_file, &mut diff_file).unwrap();
            check_file_content(&mut base_file, &expected_result);

            // 4. The diff file is bigger
            let diff_block = rand::rand_alphanumerics(block_size).into_string().unwrap();
            diff_file.write_all(diff_block.as_bytes()).unwrap();
            expected_result.append(&mut diff_block.into_bytes());
            // Rebase and check the result
            rebase(&mut base_file, &mut diff_file).unwrap();
            check_file_content(&mut base_file, &expected_result);

            // 5. The base file is bigger
            let base_block = rand::rand_alphanumerics(block_size).into_string().unwrap();
            base_file.write_all(base_block.as_bytes()).unwrap();
            expected_result.append(&mut base_block.into_bytes());
            // Rebase and check the result
            rebase(&mut base_file, &mut diff_file).unwrap();
            check_file_content(&mut base_file, &expected_result);
        }
    }
}
