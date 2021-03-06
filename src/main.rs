mod crate_name;
mod package_id_spec;
mod cache;
mod unpack;

use std::io::Read;
use anyhow::{anyhow, Context, Error};
use clap::Parser;
use tracing_subscriber::EnvFilter;
use crate::{package_id_spec::PackageIdSpec, crate_name::CrateName};

const USER_AGENT: &str = concat!("cargo-dl/", env!("CARGO_PKG_VERSION"));
const CRATE_SIZE_LIMIT: u64 = 40 * 1024 * 1024;

const SPINNER_TEMPLATE: &str = "{prefix:>40.cyan} {spinner} {msg}";
const SUCCESS_SPINNER_TEMPLATE: &str = "{prefix:>40.green} {spinner} {msg}";
const FAILURE_SPINNER_TEMPLATE: &str = "{prefix:>40.red} {spinner} {msg}";
const DOWNLOAD_TEMPLATE: &str = "{prefix:>40.cyan} {spinner} {msg}
                                   [{bar:27}] {bytes:>9}/{total_bytes:9}  {bytes_per_sec} {elapsed:>4}/{eta:4}";

#[derive(Debug, Parser)]
#[clap(bin_name = "cargo", version)]
#[clap(global_setting(clap::AppSettings::DisableHelpSubcommand))]
#[clap(global_setting(clap::AppSettings::PropagateVersion))]
enum Command {
    #[clap(about)]
    Dl(App),
}

#[derive(Debug, Parser)]
struct App {
    /// Specify this flag to have the crate extracted automatically.
    ///
    /// Note that unless changed via the --output flag, this will extract the files to a new
    /// subdirectory bearing the name of the downloaded crate archive.
    #[clap(short, long)]
    extract: bool,

    /// Normally, the compressed crate is written to a file (or directory if --extract is used)
    /// based on its name and version.  This flag allows to change that by providing an explicit
    /// file or directory path. (Only when downloading a single crate).
    #[clap(short, long)]
    output: Option<String>,

    // TODO: Easy way to download latest pre-release
    /// The crate(s) to download.
    ///
    /// Optionally including which version of the crate to download after `@`, in the standard
    /// semver constraint format used in Cargo.toml. If unspecified the newest non-prerelease,
    /// non-yanked version will be fetched.
    #[clap(name = "CRATE[@VERSION_REQ]", required = true)]
    specs: Vec<PackageIdSpec>,

    /// Allow yanked versions to be chosen.
    #[clap(long)]
    allow_yanked: bool,

    /// Disable checking cargo cache for the crate file.
    #[clap(long = "no-cache", action(clap::ArgAction::SetFalse))]
    cache: bool,

    /// Slow down operations for manually testing UI
    #[clap(long, hide = true)]
    slooooow: bool,
}

/// Failed to acquire one or more crates, see above for details
#[derive(thiserror::Error, Copy, Clone, Debug, displaydoc::Display)]
struct LoggedError;

impl App {
    fn slow(&self) {
        if self.slooooow {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }

    #[fehler::throws]
    #[tracing::instrument(fields(%self))]
    fn run(&'static self) {
        if self.specs.len() > 1 && self.output.is_some() {
            fehler::throw!(anyhow!("cannot use --output with multiple crates"));
        }

        let bars: &indicatif::MultiProgress = Box::leak(Box::new(indicatif::MultiProgress::new()));
        let spawning: &std::sync::atomic::AtomicBool = Box::leak(Box::new(std::sync::atomic::AtomicBool::new(true)));
        let thread = std::thread::spawn(move || {
            let mut index = crates_index::Index::new_cargo_default()?;
            let bar = bars.add(indicatif::ProgressBar::new_spinner()).with_style(indicatif::ProgressStyle::default_spinner().template(SPINNER_TEMPLATE))
                .with_prefix("crates.io index")
            .with_message("updating");
            bar.enable_steady_tick(100);
            index.update()?;
            self.slow();

            bar.set_style(indicatif::ProgressStyle::default_spinner().template(SUCCESS_SPINNER_TEMPLATE));
            bar.finish_with_message("updated");

            let threads = Vec::from_iter(self.specs.iter().map(|spec| {
                let bar = bars.add(indicatif::ProgressBar::new_spinner()).with_style(indicatif::ProgressStyle::default_spinner().template(SPINNER_TEMPLATE));
                (spec, std::thread::spawn(move || {
                    bar.tick();
                    bar.set_prefix(spec.to_string());
                    let index = crates_index::Index::new_cargo_default()?;
                    bar.set_message("selecting version");
                    bar.enable_steady_tick(100);
                    self.slow();
                    // TODO: fuzzy name matching https://github.com/frewsxcv/rust-crates-index/issues/75
                    let krate = match index.crate_(&spec.name.0) {
                        Some(krate) => krate,
                        None => {
                            bar.set_style(indicatif::ProgressStyle::default_spinner().template(FAILURE_SPINNER_TEMPLATE));
                            bar.finish_with_message("could not find crate in the index");
                            return Err(LoggedError.into());
                        }
                    };

                    tracing::debug!(
                        "all available versions: {:?}",
                        Vec::from_iter(krate.versions().iter().map(|v| v.version()))
                    );

                    let version_request = spec.version_req.clone().unwrap_or(semver::VersionReq::STAR);
                    let versions = {
                        let mut versions: Vec<_> = krate
                            .versions()
                            .iter()
                            .filter(|version| self.allow_yanked || !version.is_yanked())
                            .filter_map(|version| match semver::Version::parse(version.version()) {
                                Ok(num) => Some((num, version)),
                                Err(err) => {
                                    tracing::warn!(
                                        "Ignoring non-semver version {} {err:#?}",
                                        version.version()
                                    );
                                    None
                                }
                            })
                            .filter(|(num, _)| version_request.matches(num))
                            .collect();
                        versions.sort_by(|(a, _), (b, _)| a.cmp(b).reverse());
                        versions
                    };

                    tracing::debug!(
                        "matching versions: {:?}",
                        Vec::from_iter(versions.iter().map(|(num, _)| num.to_string()))
                    );

                    let (_, version) = match versions.first() {
                        Some(val) => val,
                        None => {
                            let yanked_versions = {
                                let mut versions: Vec<_> = krate
                                    .versions()
                                    .iter()
                                    .filter(|version| version.is_yanked())
                                    .filter_map(|version| match semver::Version::parse(version.version()) {
                                        Ok(num) => Some((num, version)),
                                        Err(err) => {
                                            tracing::warn!(
                                                "Ignoring non-semver version {} {err:#?}",
                                                version.version()
                                            );
                                            None
                                        }
                                    })
                                    .filter(|(num, _)| version_request.matches(num))
                                    .collect();
                                versions.sort_by(|(a, _), (b, _)| a.cmp(b).reverse());
                                versions
                            };
                            let mut msg = "no matching version found".to_owned();
                            if let Some((_, version)) = yanked_versions.first() {
                                use std::fmt::Write;
                                write!(msg, "; the yanked version {} {} matched, use `--allow-yanked` to download it", version.name(), version.version())?;
                            }
                            bar.set_style(indicatif::ProgressStyle::default_spinner().template(FAILURE_SPINNER_TEMPLATE));
                            bar.finish_with_message(msg);
                            return Err(LoggedError.into());
                        }
                    };

                    let version_str = stylish::format!("{:(fg=magenta)} {:(fg=magenta)}", version.name(), version.version());

                    let output = self.output.clone().unwrap_or_else(|| if self.extract {
                        format!("{}-{}", version.name(), version.version())
                    } else {
                        format!("{}-{}.crate", version.name(), version.version())
                    });

                    let cached = if self.cache {
                        bar.set_message(stylish::ansi::format!("checking cache for {:s}", version_str));
                        self.slow();
                        cache::lookup(&index, version)
                    } else {
                        Err(anyhow!("cache disabled by flag"))
                    };

                    match cached {
                        Ok(path) => {
                            tracing::debug!("found cached crate for {} {} at {}", version.name(), version.version(), path.display());
                            if self.extract {
                                bar.set_message(stylish::ansi::format!("extracting {:s} to {:(fg=blue)}", version_str, output));
                                let file = std::fs::File::open(path)?;
                                bar.reset();
                                bar.set_length(file.metadata()?.len());
                                bar.set_style(indicatif::ProgressStyle::default_bar().template(DOWNLOAD_TEMPLATE));
                                let archive = tar::Archive::new(flate2::bufread::GzDecoder::new(bar.wrap_read(std::io::BufReader::new(file))));
                                unpack::unpack(version, archive, &output)?;
                                self.slow();
                                bar.set_style(indicatif::ProgressStyle::default_spinner().template(SUCCESS_SPINNER_TEMPLATE));
                                bar.finish_with_message(stylish::ansi::format!("extracted {:s} to {:(fg=blue)}", version_str, output));
                            } else {
                                bar.set_message(stylish::ansi::format!("writing {:s} to {:(fg=blue)}", version_str, output));
                                self.slow();
                                std::fs::copy(path, &output)?;
                                bar.set_style(indicatif::ProgressStyle::default_spinner().template(SUCCESS_SPINNER_TEMPLATE));
                                bar.finish_with_message(stylish::ansi::format!("written {:s} to {:(fg=blue)}", version_str, output));
                            }
                        }
                        Err(err) => {
                            use sha2::Digest;
                            tracing::debug!("{err:?}");
                            let url = version.download_url(&index.index_config()?).context("missing download url")?;
                            bar.set_message(stylish::ansi::format!("downloading {:s}", version_str));
                            let resp = ureq::get(&url).set("User-Agent", USER_AGENT).call()?;
                            let mut data;
                            if let Some(len) = resp.header("Content-Length").and_then(|s| s.parse::<usize>().ok()) {
                                data = Vec::with_capacity(len);
                                bar.reset();
                                bar.set_length(u64::try_from(len)?);
                                bar.set_style(indicatif::ProgressStyle::default_bar().template(DOWNLOAD_TEMPLATE));
                            } else {
                                data = Vec::with_capacity(usize::try_from(CRATE_SIZE_LIMIT)?);
                            }
                            bar.wrap_read(resp.into_reader()).take(CRATE_SIZE_LIMIT).read_to_end(&mut data)?;
                            self.slow();
                            tracing::debug!("downloaded {} {} ({} bytes)", version.name(), version.version(), data.len());
                            bar.set_style(indicatif::ProgressStyle::default_spinner().template(SPINNER_TEMPLATE));
                            bar.set_message(stylish::ansi::format!("verifying checksum of {:s}", version_str));
                            let calculated_checksum = sha2::Sha256::digest(&data);
                            if calculated_checksum.as_slice() != version.checksum() {
                                tracing::debug!("invalid checksum, expected {} but got {}", hex::encode(version.checksum()), hex::encode(calculated_checksum));
                                bar.set_style(indicatif::ProgressStyle::default_spinner().template(FAILURE_SPINNER_TEMPLATE));
                                bar.finish_with_message("invalid checksum");
                                return Err(LoggedError.into());
                            }
                            tracing::debug!("verified checksum ({})", hex::encode(version.checksum()));
                            self.slow();

                            if self.extract {
                                bar.set_message(stylish::ansi::format!("extracting {:s} to {:(fg=blue)}", version_str, output));
                                bar.reset();
                                bar.set_length(u64::try_from(data.len())?);
                                bar.set_style(indicatif::ProgressStyle::default_bar().template(DOWNLOAD_TEMPLATE));
                                let archive = tar::Archive::new(flate2::bufread::GzDecoder::new(bar.wrap_read(std::io::Cursor::new(data))));
                                unpack::unpack(version, archive, &output)?;
                                self.slow();
                                bar.set_style(indicatif::ProgressStyle::default_spinner().template(SUCCESS_SPINNER_TEMPLATE));
                                bar.finish_with_message(stylish::ansi::format!("extracted {:s} to {:(fg=blue)}", version_str, output));
                            } else {
                                bar.set_message(stylish::ansi::format!("writing {:s} to {:(fg=blue)}", version_str, output));
                                std::fs::write(&output, data)?;
                                self.slow();
                                bar.set_style(indicatif::ProgressStyle::default_spinner().template(SUCCESS_SPINNER_TEMPLATE));
                                bar.finish_with_message(stylish::ansi::format!("written {:s} to {:(fg=blue)}", version_str, output));
                            }
                        }
                    }
                    Result::<(), anyhow::Error>::Ok(())
                }))
            }));
            spawning.store(false, std::sync::atomic::Ordering::SeqCst);
            Result::<_, anyhow::Error>::Ok(threads)
        });
        // Need to ensure all bars have been added before we stop trying to render the
        // multi-progress bar
        while spawning.load(std::sync::atomic::Ordering::SeqCst) {
            bars.join()?;
        }
        let mut logged_error = false;
        match thread.join() {
            Ok(threads) => {
                for (spec, thread) in threads? {
                    match thread.join() {
                        Ok(Ok(())) => (),
                        Ok(Err(e)) => {
                            if e.is::<LoggedError>() {
                                logged_error = true;
                            } else {
                                fehler::throw!(e.context(format!("could not acquire {}", spec)));
                            }
                        },
                        Err(e) => std::panic::resume_unwind(e),
                    }
                }
            }
            Err(e) => std::panic::resume_unwind(e),
        }
        if logged_error {
            fehler::throw!(LoggedError);
        }
    }
}

impl std::fmt::Display for App {
    #[fehler::throws(std::fmt::Error)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) {
        write!(f, "cargo dl")?;
        if self.allow_yanked {
            write!(f, " --allow-yanked")?;
        }
        if self.extract {
            write!(f, " --extract")?;
        }
        if let Some(output) = &self.output {
            write!(f, " --output={:?}", output)?;
        }
        write!(f, " --")?;
        for spec in &self.specs {
            write!(f, " {}", spec)?;
        }
    }
}

#[fehler::throws]
#[fn_error_context::context("parsing directive {:?}", directive)]
fn parse_directive(directive: &str) -> tracing_subscriber::filter::Directive {
    directive.parse()?
}

#[fehler::throws]
#[fn_error_context::context("getting directive from env var {:?}", var)]
fn get_env_directive(var: &str) -> Option<tracing_subscriber::filter::Directive> {
    if let Some(var) = std::env::var_os("CARGO_DL_LOG") {
        let s = var.to_str().context("CARGO_DL_LOG not unicode")?;
        Some(parse_directive(s)?)
    } else {
        None
    }
}

fn env_filter() -> (EnvFilter, Option<anyhow::Error>) {
    let filter = EnvFilter::new("INFO");
    match get_env_directive("CARGO_DL_LOG") {
        Ok(Some(directive)) => {
            (filter.add_directive(directive), None)
        }
        Ok(None) => {
            (filter, None)
        }
        Err(err) => {
            (filter, Some(err.context("failed to apply log directive")))
        }
    }
}

#[fehler::throws]
fn main() {
    let (env_filter, err) = env_filter();
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .pretty()
        .init();
    if let Some(err) = err {
        tracing::warn!("{err:?}");
    }
    match Command::try_parse() {
        Ok(Command::Dl(app)) => Box::leak(Box::new(app)).run()?,
        Err(e @ clap::Error { kind: clap::ErrorKind::ValueValidation, .. }) => {
            use std::error::Error;
            println!("Error: invalid value for {}", e.info[0]);
            println!();
            if let Some(source) = e.source() {
                println!("Caused by:");
                let chain = anyhow::Chain::new(source);
                for (i, error) in chain.into_iter().enumerate() {
                    println!("    {i}: {error}");
                }
            }
            std::process::exit(1);
        }
        Err(e) => e.exit(),
    }
}
