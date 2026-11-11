use std::{env, error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let library = pkg_config::Config::new()
        .atleast_version("1.23")
        .probe("libva")?;
    pkg_config::probe_library("libva-drm")?;
    let functions = [
        "vaGetDisplayDRM",
        "vaInitialize",
        "vaTerminate",
        "vaErrorStr",
        "vaQueryVendorString",
        "vaMaxNumEntrypoints",
        "vaQueryConfigEntrypoints",
        "vaCreateConfig",
        "vaDestroyConfig",
        "vaQuerySurfaceAttributes",
        "vaCreateSurfaces",
        "vaDestroySurfaces",
        "vaSyncSurface",
        "vaExportSurfaceHandle",
        "vaCreateContext",
        "vaDestroyContext",
        "vaCreateBuffer",
        "vaDestroyBuffer",
        "vaBeginPicture",
        "vaRenderPicture",
        "vaEndPicture",
        "vaMaxNumImageFormats",
        "vaQueryImageFormats",
        "vaCreateImage",
        "vaDestroyImage",
        "vaGetImage",
        "vaPutImage",
        "vaMapBuffer",
        "vaUnmapBuffer",
    ];
    let mut builder = bindgen::Builder::default()
        .header_contents("weld-vaapi.h", "#include <va/va.h>\n#include <va/va_drm.h>\n#include <va/va_drmcommon.h>\n#include <va/va_vpp.h>\n")
        .allowlist_function(format!("^({})$", functions.join("|")))
        .allowlist_type("VAProcPipelineParameterBuffer|VADRMPRIMESurfaceDescriptor|VADRMFormatModifierList")
        .allowlist_var("VA_(STATUS_SUCCESS|STATUS_ERROR_UNSUPPORTED_PROFILE|RT_FORMAT_RGB32|RT_FORMAT_YUV420|FOURCC_BGRX|FOURCC_NV12|SURFACE_ATTRIB_SETTABLE|SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2|SURFACE_ATTRIB_USAGE_HINT_VPP_READ|SURFACE_ATTRIB_USAGE_HINT_VPP_WRITE|SURFACE_ATTRIB_USAGE_HINT_EXPORT|EXPORT_SURFACE_READ_ONLY|EXPORT_SURFACE_COMPOSED_LAYERS|PROGRESSIVE|GENERIC_VALUE_TYPE_INTEGER|GENERIC_VALUE_TYPE_POINTER)")
        .constified_enum_module("VA.*")
        .derive_default(true).layout_tests(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for directory in library.include_paths {
        builder = builder.clang_arg(format!("-I{}", directory.display()));
    }
    builder
        .generate()?
        .write_to_file(PathBuf::from(env::var("OUT_DIR")?).join("bindings.rs"))?;
    Ok(())
}
