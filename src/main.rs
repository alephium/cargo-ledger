use cargo_metadata::Message;
use clap::{Parser, Subcommand, ValueEnum};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;

use serde_json::json;

mod setup;
mod utils;

use serde_derive::Deserialize;
use setup::install_targets;
use utils::*;

#[derive(Debug, Deserialize)]
struct NanosMetadata {
    curve: Vec<String>,
    path: Vec<String>,
    flags: String,
    icon: String,
    icon_small: String,
    name: Option<String>,
}

#[derive(Parser, Debug)]
#[command(name = "cargo")]
#[command(bin_name = "cargo")]
#[clap(name = "Ledger devices build and load commands")]
#[clap(version = "0.0")]
#[clap(about = "Builds the project and emits a JSON manifest for ledgerctl.")]
enum Cli {
    Ledger(CliArgs),
}

#[derive(clap::Args, Debug)]
struct CliArgs {
    #[clap(long)]
    #[clap(value_name = "prebuilt ELF exe")]
    use_prebuilt: Option<std::path::PathBuf>,

    #[clap(long)]
    #[clap(help = concat!(
        "Should the app.hex be placed next to the app.json, or next to the input exe?",
        " ",
        "Typically used with --use-prebuilt when the input exe is in a read-only location.",
    ))]
    hex_next_to_json: bool,

    #[clap(subcommand)]
    command: MainCommand,
}

#[derive(ValueEnum, Clone, Debug)]
enum Device {
    Nanos,
    Nanox,
    Nanosplus,
}

impl AsRef<str> for Device {
    fn as_ref(&self) -> &str {
        match self {
            Device::Nanos => "nanos",
            Device::Nanox => "nanox",
            Device::Nanosplus => "nanosplus",
        }
    }
}

#[derive(Subcommand, Debug)]
enum MainCommand {
    #[clap(about = "install custom target files")]
    Setup,
    #[clap(about = "build the project for a given device")]
    Build {
        #[clap(value_enum)]
        #[clap(help = "device to build for")]
        device: Device,
        #[clap(short, long)]
        #[clap(help = "load on a device")]
        load: bool,
        #[clap(last = true)]
        remaining_args: Vec<String>,
    },
}

fn main() {
    let Cli::Ledger(cli) = Cli::parse();

    match cli.command {
        MainCommand::Setup => install_targets(),
        MainCommand::Build {
            device: d,
            load: a,
            remaining_args: r,
        } => {
            build_app(d, a, cli.use_prebuilt, cli.hex_next_to_json, r);
        }
    }
}

fn build_app(
    device: Device,
    is_load: bool,
    use_prebuilt: Option<PathBuf>,
    hex_next_to_json: bool,
    remaining_args: Vec<String>,
) {
    let ledger_target_path = match env::var("LEDGER_TARGETS") {
        Ok(path) => path,
        Err(_) => String::new(),
    };
    let device_str = device.as_ref();
    let device_json = format!("{}.json", &device_str);
    let device_json_path = Path::new(&ledger_target_path).join(&device_json);
    println!("Using target file: {}", device_json_path.display());

    let exe_path = match use_prebuilt {
        None => {
            let mut cargo_cmd = Command::new("cargo")
                .args([
                    "build",
                    "--release",
                    format!("--target={}", device_json_path.display()).as_str(),
                    "--message-format=json-diagnostic-rendered-ansi",
                ])
                .args(&remaining_args)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();

            let mut exe_path = std::path::PathBuf::new();
            let out = cargo_cmd.stdout.take().unwrap();
            let reader = std::io::BufReader::new(out);
            for message in cargo_metadata::Message::parse_stream(reader) {
                match message.as_ref().unwrap() {
                    Message::CompilerArtifact(artifact) => {
                        if let Some(n) = &artifact.executable {
                            exe_path = n.to_path_buf();
                        }
                    }
                    Message::CompilerMessage(message) => {
                        println!("{message}");
                    }
                    _ => (),
                }
            }

            cargo_cmd.wait().expect("Couldn't get cargo's exit status");

            exe_path
        }
        Some(prebuilt) => prebuilt,
    };

    // Fetch crate metadata without fetching dependencies
    let mut cmd = cargo_metadata::MetadataCommand::new();
    let res = cmd.no_deps().exec().unwrap();

    // Fetch package.metadata.nanos section
    let this_pkg = res.packages.last().unwrap();
    let metadata_value = this_pkg
        .metadata
        .get("nanos")
        .expect("package.metadata.nanos section is missing in Cargo.toml")
        .clone();
    let this_metadata: NanosMetadata =
        serde_json::from_value(metadata_value).unwrap();

    let current_dir = this_pkg.manifest_path.parent().unwrap();

    let hex_file_abs = if hex_next_to_json {
        current_dir
    } else {
        exe_path.parent().unwrap()
    }
    .join("app.hex");

    export_binary(&exe_path, &hex_file_abs);

    // app.json will be placed in the app's root directory
    let app_json_name = format!("app_{}.json", device.as_ref());
    let app_json = current_dir.join(app_json_name);

    // Find hex file path relative to 'app.json'
    let hex_file = hex_file_abs.strip_prefix(current_dir).unwrap();

    // Retrieve real data size and SDK infos from ELF
    let infos = retrieve_infos(&exe_path).unwrap();

    // Modify flags to enable BLE if targetting Nano X
    let flags = match device {
        Device::Nanos | Device::Nanosplus => this_metadata.flags,
        Device::Nanox => {
            let base = u32::from_str_radix(this_metadata.flags.as_str(), 16)
                .unwrap_or(0);
            format!("0x{:x}", base | 0x200)
        }
    };

    // Pick icon and targetid according to target
    let (targetid, icon) = match device {
        Device::Nanos => ("0x31100004", &this_metadata.icon),
        Device::Nanox => ("0x33000004", &this_metadata.icon_small),
        Device::Nanosplus => ("0x33100004", &this_metadata.icon_small),
    };

    // create manifest
    let file = fs::File::create(&app_json).unwrap();
    let mut json = json!({
        "name": this_metadata.name.as_ref().unwrap_or(&this_pkg.name),
        "version": &this_pkg.version,
        "icon": icon,
        "targetId": targetid,
        "flags": flags,
        "derivationPath": {
            "curves": this_metadata.curve,
            "paths": this_metadata.path
        },
        "binary": hex_file,
        "dataSize": infos.size
    });
    // Ignore apiLevel for Nano S as it is unsupported for now
    match device {
        Device::Nanos => (),
        _ => {
            json["apiLevel"] = infos.api_level.to_string().into();
        }
    }
    serde_json::to_writer_pretty(file, &json).unwrap();

    if is_load {
        install_with_ledgerctl(current_dir, &app_json);
    }
}
