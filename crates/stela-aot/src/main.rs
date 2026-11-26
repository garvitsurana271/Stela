//! `stela-aot`: analyze the images an ahead-of-time translation covers.
//!
//! ```text
//! stela-aot analyze (--kernel <collection> | --cache <main file>)
//!     [--samples <file>]... [--top <n>] [--extract <file> [--extract-top <n>]]
//! ```
//!
//! `analyze` prints the [`stela_aot::census`] of the images, weighted by
//! the samples given. `--extract` writes the units of the hottest sampled
//! functions as `pc weight hex-bytes` lines, the input of the compiler's
//! hosted AOT differential and density runs (`block::aot`): Apple's bytes,
//! so the file must lie outside the repository.
use anyhow::{Context, Result, bail};
use stela_aot::census::{self, Census};
use stela_aot::image::{self, CacheFiles, Mapped};
use std::fmt::Write as _;
use std::path::PathBuf;

struct Arguments {
    kernel: Option<PathBuf>,
    cache: Option<PathBuf>,
    samples: Vec<PathBuf>,
    top: usize,
    extract: Option<PathBuf>,
    extract_top: usize,
}

fn arguments() -> Result<Arguments> {
    let mut arguments = Arguments {
        kernel: None,
        cache: None,
        samples: Vec::new(),
        top: 30,
        extract: None,
        extract_top: 200,
    };
    let mut words = std::env::args().skip(1);
    match words.next().as_deref() {
        Some("analyze") => {}
        _ => bail!(
            "usage: stela-aot analyze (--kernel <collection> | --cache <main file>) \
             [--samples <file>]... [--top <n>] [--extract <file> [--extract-top <n>]]"
        ),
    }
    while let Some(word) = words.next() {
        let mut value = || words.next().with_context(|| format!("{word} needs a value"));
        match word.as_str() {
            "--kernel" => arguments.kernel = Some(value()?.into()),
            "--cache" => arguments.cache = Some(value()?.into()),
            "--samples" => arguments.samples.push(value()?.into()),
            "--top" => arguments.top = value()?.parse()?,
            "--extract" => arguments.extract = Some(value()?.into()),
            "--extract-top" => arguments.extract_top = value()?.parse()?,
            _ => bail!("unknown argument {word}"),
        }
    }
    if arguments.kernel.is_some() == arguments.cache.is_some() {
        bail!("name exactly one of --kernel and --cache");
    }
    Ok(arguments)
}

/// Refuse to write Apple's bytes inside a git work tree.
fn refuse_repository(path: &std::path::Path) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    let mut at = absolute.parent();
    while let Some(directory) = at {
        if directory.join(".git").exists() {
            bail!(
                "--extract {} is inside a git work tree ({}): Apple's bytes stay outside it",
                path.display(),
                directory.display()
            );
        }
        at = directory.parent();
    }
    Ok(())
}

fn main() -> Result<()> {
    let arguments = arguments()?;
    if let Some(extract) = &arguments.extract {
        refuse_repository(extract)?;
    }
    let mut samples = Vec::new();
    for path in &arguments.samples {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        samples.extend(census::read_samples(&text));
    }
    // Both files stay mapped while their images are in use.
    let kernel;
    let cache;
    let (uuid, images) = if let Some(path) = &arguments.kernel {
        kernel = Mapped::open(path)?;
        image::kernel_images(&kernel)?
    } else {
        cache = CacheFiles::open(arguments.cache.as_ref().expect("a cache"))?;
        cache.images()?
    };
    println!(
        "uuid {} images {}",
        uuid.as_ref()
            .map_or_else(|| String::from("none"), image::uuid_text),
        images.len()
    );
    let places = census::places(&images);
    let census = Census::of(&images, &places, &samples);
    print!("{}", census.report(&images, &places, arguments.top));
    if let Some(path) = &arguments.extract {
        let mut out = String::new();
        let mut written = 0;
        for (weight, at) in census.hottest(arguments.extract_top) {
            let place = &places[at];
            let Some(bytes) = images[place.image].bytes(place.start, 4 * place.unit_words())
            else {
                continue;
            };
            let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            let _ = writeln!(out, "{:x} {weight} {hex}", place.start);
            written += 1;
        }
        std::fs::write(path, out).with_context(|| format!("write {}", path.display()))?;
        println!("extracted {written} units to {}", path.display());
    }
    Ok(())
}
