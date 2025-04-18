use std::io::{BufRead as _, BufReader, Write as _};
use std::path::PathBuf;

use clap::Parser;

mod args;
mod report;

use proc_exit::prelude::*;

use typos_cli::report::Report;

fn main() {
    human_panic::setup_panic!();
    let result = run();
    proc_exit::exit(result);
}

fn run() -> proc_exit::ExitResult {
    // clap's `get_matches` uses Failure rather than Usage, so bypass it for `get_matches_safe`.
    let args = match args::Args::try_parse() {
        Ok(args) => args,
        Err(e) if e.use_stderr() => {
            let _ = e.print();
            return proc_exit::sysexits::USAGE_ERR.ok();
        }
        Err(e) => {
            let _ = e.print();
            return proc_exit::Code::SUCCESS.ok();
        }
    };

    args.color.write_global();

    init_logging(args.verbose.log_level());

    if let Some(output_path) = args.dump_config.as_ref() {
        run_dump_config(&args, output_path)
    } else if args.type_list {
        run_type_list(&args)
    } else {
        run_checks(&args)
    }
}

fn run_dump_config(args: &args::Args, output_path: &std::path::Path) -> proc_exit::ExitResult {
    let global_cwd = std::env::current_dir().to_sysexits()?;

    let path = &args.path[0];
    let cwd = if path == std::path::Path::new("-") {
        global_cwd
    } else if path.is_file() {
        let mut cwd = path
            .canonicalize()
            .with_code(proc_exit::sysexits::USAGE_ERR)?;
        cwd.pop();
        cwd
    } else {
        path.canonicalize()
            .with_code(proc_exit::sysexits::USAGE_ERR)?
    };
    let cwd = cwd
        .canonicalize()
        .with_code(proc_exit::sysexits::USAGE_ERR)?;

    let storage = typos_cli::policy::ConfigStorage::new();
    let mut engine = typos_cli::policy::ConfigEngine::new(&storage);
    engine.set_isolated(args.isolated);

    let mut overrides = typos_cli::config::Config::default();
    if let Some(path) = args.custom_config.as_ref() {
        let custom = typos_cli::config::Config::from_file(path)
            .with_code(proc_exit::sysexits::CONFIG_ERR)?;
        if let Some(custom) = custom {
            overrides.update(&custom);
        }
    }
    overrides.update(&args.config.to_config());
    engine.set_overrides(overrides);

    let config = engine
        .load_config(&cwd)
        .with_code(proc_exit::sysexits::CONFIG_ERR)?;

    let mut defaulted_config = typos_cli::config::Config::from_defaults();
    defaulted_config.update(&config);
    let output = toml::to_string_pretty(&defaulted_config).with_code(proc_exit::Code::FAILURE)?;
    if output_path == std::path::Path::new("-") {
        std::io::stdout()
            .write_all(output.as_bytes())
            .to_sysexits()?;
    } else {
        std::fs::write(output_path, &output).to_sysexits()?;
    }

    Ok(())
}

fn run_type_list(args: &args::Args) -> proc_exit::ExitResult {
    let global_cwd = std::env::current_dir().to_sysexits()?;

    let path = &args.path[0];
    let cwd = if path == std::path::Path::new("-") {
        global_cwd
    } else if path.is_file() {
        let mut cwd = path
            .canonicalize()
            .with_code(proc_exit::sysexits::USAGE_ERR)?;
        cwd.pop();
        cwd
    } else {
        path.canonicalize()
            .with_code(proc_exit::sysexits::USAGE_ERR)?
    };
    let cwd = cwd
        .canonicalize()
        .with_code(proc_exit::sysexits::USAGE_ERR)?;

    let storage = typos_cli::policy::ConfigStorage::new();
    let mut engine = typos_cli::policy::ConfigEngine::new(&storage);
    engine.set_isolated(args.isolated);

    let mut overrides = typos_cli::config::Config::default();
    if let Some(path) = args.custom_config.as_ref() {
        let custom = typos_cli::config::Config::from_file(path)
            .with_code(proc_exit::sysexits::CONFIG_ERR)?;
        if let Some(custom) = custom {
            overrides.update(&custom);
        }
    }
    overrides.update(&args.config.to_config());
    engine.set_overrides(overrides);

    engine
        .init_dir(&cwd)
        .with_code(proc_exit::sysexits::CONFIG_ERR)?;
    let definitions = engine.file_types(&cwd);

    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    for (name, globs) in definitions {
        writeln!(handle, "{}: {}", name, itertools::join(globs, ", ")).to_sysexits()?;
    }

    Ok(())
}

fn run_checks(args: &args::Args) -> proc_exit::ExitResult {
    let global_cwd = std::env::current_dir()
        .map_err(|err| {
            let kind = err.kind();
            std::io::Error::new(kind, "no current working directory".to_owned())
        })
        .to_sysexits()?;

    let storage = typos_cli::policy::ConfigStorage::new();
    let mut engine = typos_cli::policy::ConfigEngine::new(&storage);
    engine.set_isolated(args.isolated);

    let mut overrides = typos_cli::config::Config::default();
    if let Some(path) = args.custom_config.as_ref() {
        let custom = typos_cli::config::Config::from_file(path)
            .with_code(proc_exit::sysexits::CONFIG_ERR)?;
        if let Some(custom) = custom {
            overrides.update(&custom);
        }
    }
    overrides.update(&args.config.to_config());
    engine.set_overrides(overrides);

    let mut typos_found = false;
    let mut errors_found = false;

    let file_list = match args.file_list.as_deref() {
        Some(dash) if dash == PathBuf::from("-") => Some(
            std::io::stdin()
                .lines()
                .map(|res| res.map(PathBuf::from))
                .collect::<Result<_, _>>()
                .with_code(proc_exit::sysexits::IO_ERR)?,
        ),
        Some(path) => Some(
            BufReader::new(std::fs::File::open(path).with_code(proc_exit::sysexits::IO_ERR)?)
                .lines()
                .map(|res| res.map(PathBuf::from))
                .collect::<Result<_, _>>()
                .with_code(proc_exit::sysexits::IO_ERR)?,
        ),
        None => None,
    };

    // HACK: Diff doesn't handle mixing content
    let global_reporter = if args.diff {
        Box::new(report::PrintSilent)
    } else {
        args.format.reporter()
    };

    // Note: file_list and args.path are mutually exclusive, enforced by clap
    'path: for path in file_list.as_ref().unwrap_or(&args.path) {
        // Note paths are passed through stdin, `-` is treated like a normal path
        let cwd = if path == std::path::Path::new("-") {
            if args.file_list.is_some() {
                return Err(proc_exit::sysexits::USAGE_ERR.with_message(
                    "Can't use `-` (stdin) while using `--file_list` provided paths",
                ));
            };
            global_cwd.clone()
        } else if path.is_file() {
            let mut cwd = path
                .canonicalize()
                .map_err(|err| {
                    let kind = err.kind();
                    std::io::Error::new(kind, format!("argument `{}` is not found", path.display()))
                })
                .with_code(proc_exit::sysexits::USAGE_ERR)?;
            cwd.pop();
            cwd
        } else {
            path.canonicalize()
                .map_err(|err| {
                    let kind = err.kind();
                    std::io::Error::new(kind, format!("argument `{}` is not found", path.display()))
                })
                .with_code(proc_exit::sysexits::USAGE_ERR)?
        };

        engine
            .init_dir(&cwd)
            .with_code(proc_exit::sysexits::CONFIG_ERR)?;
        let walk_policy = engine.walk(&cwd);

        let threads = if path.is_file() || args.sort {
            1
        } else {
            args.threads
        };
        let single_threaded = threads == 1;

        let mut walk = ignore::WalkBuilder::new(path);
        walk.threads(threads)
            .skip_stdout(true)
            .hidden(walk_policy.ignore_hidden())
            .ignore(walk_policy.ignore_dot())
            .git_global(walk_policy.ignore_global())
            .git_ignore(walk_policy.ignore_vcs())
            .git_exclude(walk_policy.ignore_vcs())
            .parents(walk_policy.ignore_parent());
        if args.sort {
            walk.sort_by_file_name(|a, b| a.cmp(b));
        }
        if !walk_policy.extend_exclude.is_empty() {
            let mut ignores = ignore::gitignore::GitignoreBuilder::new(".");
            for pattern in walk_policy.extend_exclude.iter() {
                ignores
                    .add_line(None, pattern)
                    .with_code(proc_exit::sysexits::CONFIG_ERR)?;
            }
            let ignores = ignores.build().with_code(proc_exit::sysexits::CONFIG_ERR)?;
            if args.force_exclude {
                let mut ancestors = path.ancestors().collect::<Vec<_>>();
                ancestors.reverse();
                for path in ancestors {
                    match ignores.matched(path, path.is_dir()) {
                        ignore::Match::None => {}
                        ignore::Match::Ignore(_) => continue 'path,
                        ignore::Match::Whitelist(_) => break,
                    }
                }
            }
            walk.filter_entry(move |entry| {
                let path = entry.path();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let matched = ignores.matched(path, is_dir);
                log::debug!("match({path:?}, {is_dir}) == {matched:?}");
                match matched {
                    ignore::Match::None => true,
                    ignore::Match::Ignore(_) => false,
                    ignore::Match::Whitelist(_) => true,
                }
            });
        }

        let status_reporter = report::MessageStatus::new(global_reporter.as_ref());
        let reporter: &dyn Report = &status_reporter;

        let selected_checks: &dyn typos_cli::file::FileChecker = if args.files {
            &typos_cli::file::FoundFiles
        } else if args.file_types {
            &typos_cli::file::FileTypes
        } else if args.highlight_identifiers {
            &typos_cli::file::HighlightIdentifiers
        } else if args.identifiers {
            &typos_cli::file::Identifiers
        } else if args.highlight_words {
            &typos_cli::file::HighlightWords
        } else if args.words {
            &typos_cli::file::Words
        } else if args.write_changes {
            &typos_cli::file::FixTypos
        } else if args.diff {
            &typos_cli::file::DiffTypos
        } else {
            &typos_cli::file::Typos
        };

        if single_threaded {
            typos_cli::file::walk_path(
                walk.build(),
                selected_checks,
                &engine,
                reporter,
                args.force_exclude,
            )
        } else {
            typos_cli::file::walk_path_parallel(
                walk.build_parallel(),
                selected_checks,
                &engine,
                reporter,
                args.force_exclude,
            )
        }
        .map_err(|e| {
            e.io_error()
                .map(|i| {
                    let kind = i.kind();
                    proc_exit::sysexits::io_to_sysexists(kind)
                        .or_else(|| proc_exit::bash::io_to_signal(kind))
                        .unwrap_or(proc_exit::sysexits::IO_ERR)
                })
                .unwrap_or_default()
                .with_message(e)
        })?;
        if status_reporter.typos_found() {
            typos_found = true;
        }
        if status_reporter.errors_found() {
            errors_found = true;
        }
    }

    if let Err(err) = global_reporter.generate_final_result() {
        errors_found = true;
        log::error!("could not render end-report: {err}");
    }

    if errors_found {
        proc_exit::Code::FAILURE.ok()
    } else if typos_found {
        // Can;'t use `Failure` since its so prevalent, it could be easy to get a
        // `Failure` from something else and get it mixed up with typos.
        //
        // Can't use DataErr or anything else an std::io::ErrorKind might map to.
        proc_exit::Code::new(2).ok()
    } else {
        proc_exit::Code::SUCCESS.ok()
    }
}

fn init_logging(level: Option<log::Level>) {
    if let Some(level) = level {
        let mut builder = env_logger::Builder::new();

        let choice = anstream::AutoStream::choice(&std::io::stderr());
        builder.write_style(if matches!(choice, anstream::ColorChoice::Never) {
            env_logger::WriteStyle::Never
        } else {
            env_logger::WriteStyle::Always
        });

        builder.filter(None, level.to_level_filter());

        if level == log::LevelFilter::Trace {
            builder.format_timestamp_secs();
        } else {
            builder.format(|f, record| {
                writeln!(
                    f,
                    "[{}] {}",
                    record.level().to_string().to_lowercase(),
                    record.args()
                )
            });
        }

        builder.init();
    }
}
