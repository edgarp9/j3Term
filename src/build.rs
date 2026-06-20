fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("icon.ico");
        if let Err(error) = resource.compile() {
            if can_skip_missing_cross_resource_tool(&error) {
                println!(
                    "cargo:warning=skipping Windows icon resource for debug cross-check: {error}"
                );
            } else {
                return Err(error.into());
            }
        }
    }

    Ok(())
}

fn can_skip_missing_cross_resource_tool(error: &std::io::Error) -> bool {
    if error.kind() != std::io::ErrorKind::NotFound {
        return false;
    }

    if std::env::var("PROFILE").as_deref() == Ok("release") {
        return false;
    }

    let host = std::env::var("HOST").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    !host.is_empty() && !target.is_empty() && host != target
}
