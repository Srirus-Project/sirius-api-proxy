#[path = "src/proto_source.rs"]
mod proto_source;
#[allow(dead_code)]
#[path = "src/routes.rs"]
mod routes;
use heck::{ToSnakeCase, ToUpperCamelCase};
use prost::Message;
use std::{env, fs, path::PathBuf};

fn rust_type(message: prost_reflect::MessageDescriptor) -> String {
    assert!(
        message.parent_message().is_none(),
        "RPC types must be top-level"
    );
    format!(
        "{}::{}",
        message
            .package_name()
            .split('.')
            .map(ToSnakeCase::to_snake_case)
            .collect::<Vec<_>>()
            .join("::"),
        message.name().to_upper_camel_case()
    )
}
fn main() {
    generate("jp", "protocol/sirius/1.0.3");
    generate("global", "protocol/global/1.0.1");
}
fn generate(family: &str, path: &str) {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let bundle = root.join(path);
    println!("cargo:rerun-if-changed={}", bundle.display());
    println!("cargo:rerun-if-changed=src/proto_source.rs");
    println!("cargo:rerun-if-changed=src/routes.rs");
    let compiled = proto_source::compile(&bundle).expect("compile built-in proto bundle");
    let pool = prost_reflect::DescriptorPool::decode(compiled.encoded.as_slice()).unwrap();
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join(family);
    fs::create_dir_all(&out).unwrap();
    prost_build::Config::new()
        .out_dir(&out)
        .compile_well_known_types()
        .extern_path(".google.protobuf", "::pbjson_types")
        .include_file("messages.rs")
        .compile_fds(prost_types::FileDescriptorSet::decode(compiled.encoded.as_slice()).unwrap())
        .expect("generate native protobuf types");
    pbjson_build::Builder::new()
        .out_dir(&out)
        .register_descriptors(&compiled.encoded)
        .unwrap()
        .extern_path(".google.protobuf", "::pbjson_types")
        .build(&[".app", ".entity"])
        .expect("generate protobuf JSON codecs");
    // Each generated serde implementation belongs beside its message types.
    let packages: std::collections::BTreeSet<_> = pool
        .files()
        .map(|file| file.package_name().to_owned())
        .filter(|package| {
            package == "app"
                || package.starts_with("app.")
                || package == "entity"
                || package.starts_with("entity.")
        })
        .collect();
    for package in packages {
        let name = format!("{package}.serde.rs");
        if out.join(&name).exists() {
            let types = out.join(format!("{package}.rs"));
            let mut content = fs::read_to_string(&types).unwrap();
            content.push_str(&format!(
                "\ninclude!(concat!(env!(\"OUT_DIR\"), \"/{family}/{name}\"));\n"
            ));
            fs::write(types, content).unwrap();
        }
    }
    let mut encode = String::new();
    let mut decode = String::new();
    for route in routes::contract_for_family(family) {
        let (service, method) = route.trim_start_matches('/').rsplit_once('/').unwrap();
        let method = pool
            .get_service_by_name(service)
            .unwrap()
            .methods()
            .find(|m| m.name() == method)
            .unwrap();
        encode.push_str(&format!(
            "{route:?} => encode_message::<{}>(value),\n",
            rust_type(method.input())
        ));
        decode.push_str(&format!(
            "{route:?} => decode_message::<{}>(bytes),\n",
            rust_type(method.output())
        ));
    }
    fs::write(
        out.join("dispatch.rs"),
        format!(
            r#"
        pub const SHA256: &str = {:?};
        pub fn encode(route: &str, value: &serde_json::Value) -> Result<Option<Vec<u8>>, AppError> {{
            match route {{ {} _ => Err(AppError::ProtocolDefinition) }}
        }}
        pub fn decode(route: &str, bytes: &[u8]) -> Result<Option<serde_json::Value>, AppError> {{
            match route {{ {} _ => Err(AppError::ProtocolDefinition) }}
        }}
    "#,
            compiled.sha256, encode, decode
        ),
    )
    .unwrap();
    let messages = out.join("messages.rs");
    // Keep generated includes relative to OUT_DIR for reproducible archives.
    let content = fs::read_to_string(&messages).unwrap();
    let content = content.replace(
        &format!("include!(\"{}/", out.display()),
        &format!("include!(concat!(env!(\"OUT_DIR\"), \"/{family}/"),
    );
    let content = content
        .lines()
        .map(|line| {
            if line.contains("include!(concat!") && !line.ends_with("));") {
                line.replace("\");", "\"));")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(messages, content).unwrap();
    assert_eq!(compiled.family, family);
    // Used here as well as runtime, so every field of the shared result is checked.
    assert!(!compiled.version.is_empty() && compiled.files > 0);
}
