#[path = "../../packaging/windows/resources.rs"]
mod resources;

#[path = "src/i18n/catalog.rs"]
mod catalog;
#[path = "src/i18n/template.rs"]
mod template;

fn embed_locales() -> Result<(), String> {
    use std::fmt::Write;
    use std::path::PathBuf;

    let base = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("src/i18n");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/i18n/catalog.rs");
    println!("cargo:rerun-if-changed=src/i18n/template.rs");
    println!(
        "cargo:rerun-if-changed={}",
        base.join("locales.json").display()
    );
    let registry = std::fs::read_to_string(base.join("locales.json")).map_err(|e| e.to_string())?;
    let locales = catalog::load(&registry, |file| {
        let path = base.join("locales").join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        std::fs::read_to_string(path).map_err(|e| format!("{file}: {e}"))
    })?;
    let mut output = String::from("// Generated from i18n/locales.json and its resource files.\n");
    writeln!(
        output,
        "const EN_INDEX: usize = {};",
        locales
            .iter()
            .position(|locale| locale.code == "en")
            .unwrap()
    )
    .unwrap();
    writeln!(
        output,
        "const ZH_CN_INDEX: usize = {};",
        locales
            .iter()
            .position(|locale| locale.code == "zh-CN")
            .unwrap()
    )
    .unwrap();
    output.push_str("static ALL_LANGS: &[Lang] = &[\n");
    for index in 0..locales.len() {
        writeln!(output, "    Lang({index}),").unwrap();
    }
    output.push_str("];\nstatic LOCALES: &[Locale] = &[\n");
    for locale in &locales {
        let fallback = locale
            .fallback
            .as_ref()
            .map(|code| locales.iter().position(|item| &item.code == code).unwrap());
        writeln!(output, "    Locale {{ code: {:?}, native_name: {:?}, aliases: &{:?}, fallback: {:?}, messages: &[", locale.code, locale.native_name, locale.aliases, fallback).unwrap();
        for (key, text) in &locale.messages {
            writeln!(output, "        ({key:?}, {text:?}),").unwrap();
        }
        output.push_str("    ] },\n");
    }
    output.push_str("];\n");
    let destination = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("locales.rs");
    std::fs::write(destination, output).map_err(|e| e.to_string())
}

fn main() {
    embed_locales().unwrap_or_else(|error| panic!("Invalid translation resources: {error}"));
    embed_sensor_component();
    resources::embed("tramoing", "Tramoing 托盘流量监控");
}

fn embed_sensor_component() {
    println!("cargo:rerun-if-env-changed=TRAMOING_SENSOR_COMPONENT_MANIFEST");
    let content = match std::env::var_os("TRAMOING_SENSOR_COMPONENT_MANIFEST") {
        Some(path) => {
            println!(
                "cargo:rerun-if-changed={}",
                std::path::Path::new(&path).display()
            );
            let text = std::fs::read_to_string(path).expect("read sensor component manifest");
            let value: serde_json::Value =
                serde_json::from_str(&text).expect("valid sensor manifest");
            assert_eq!(value["protocol"].as_u64(), Some(1));
            assert_eq!(value["architecture"].as_str(), Some("x64"));
            assert!(value["size"].as_u64().is_some_and(|size| size > 0));
            assert!(value["sha256"].as_str().is_some_and(
                |hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
            ));
            text
        }
        // Local development and ARM64 builds work without a published component.
        // Production x64 packaging always supplies a verified manifest.
        None => "null".into(),
    };
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("sensors-component.json"), content).expect("embed sensor manifest");
}
