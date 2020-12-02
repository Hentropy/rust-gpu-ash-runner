use std::{
    fs::File,
    path::PathBuf,
    process::{Command, Stdio},
};

use ash::util::read_spv;

use serde::Deserialize;

pub fn compile_shaders() -> Vec<SpirvShader> {
    // Check if/what needs rebuild
    // (cargo might just handle this on its own? ignore for now)

    let spirv_codegen_backend = String::from("codegen_backend=rustc_codegen_spirv.dll");
    let rustflags = format!("-Z {} -Z symbol-mangling-version=v0", spirv_codegen_backend);
    let manifest_path = "shaders\\Cargo.toml";
    let target_dir = "shaders\\target";

    // run a cargo process with spirv codegen
    let cargo_out = Command::new("cargo")
        .args(&["build", "--release"])
        .arg("--target-dir")
        .arg(target_dir)
        .arg("--manifest-path")
        .arg(manifest_path)
        .args(&["--target", "spirv-unknown-unknown"])
        .args(&["--message-format", "json-render-diagnostics"])
        .args(&["-Z", "build-std=core"])
        .env("RUSTFLAGS", rustflags)
        .stderr(Stdio::inherit())
        .output()
        .expect("cargo failed to execute build");

    // parse the json output from cargo to get the artifact paths
    let spv_paths: Vec<PathBuf> = String::from_utf8(cargo_out.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| match serde_json::from_str::<SpirvArtifacts>(line) {
            Ok(line) => Some(line),
            Err(_) => None,
        })
        .filter(|line| line.reason == "compiler-artifact")
        .last()
        .expect("No output artifacts")
        .filenames
        .expect("No artifact filenemaes")
        .into_iter()
        .filter(|filename| filename.ends_with(".spv"))
        .map(Into::into)
        .collect();

    // load the spirv data into memory
    let mut artifacts = Vec::<SpirvShader>::with_capacity(spv_paths.len());
    for path in spv_paths {
        let name = path.file_stem().unwrap().to_owned().into_string().unwrap();
        let mut file = File::open(path).unwrap();
        let spirv = read_spv(&mut file).unwrap();
        //let mut loader = rspirv::dr::Loader::new();
        //rspirv::binary::parse_words(&spirv, &mut loader).expect("Invalid spirv module");
        //let module = loader.module();
        artifacts.push(SpirvShader { name, spirv });
    }

    artifacts
}

#[derive(Deserialize)]
struct SpirvArtifacts {
    reason: String,
    filenames: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct SpirvShader {
    pub name: String,
    pub spirv: Vec<u32>,
}
