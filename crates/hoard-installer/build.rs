fn main() {
    // `DefaultTranslationContext::None` so a string is keyed by the string
    // itself and not by the component it happens to sit in. Without it, moving
    // "Close" from one screen to another silently orphans its translation.
    let config = slint_build::CompilerConfiguration::new()
        .with_bundled_translations("lang")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/installer.slint", config)
        .expect("compiling the installer UI");

    // The file icon, and the details Windows shows under Properties. Both are
    // PE resources, so they are stamped at link time and not settable from the
    // UI at all.
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("icons/icon.ico");
        res.set("ProductName", "Hoard");
        res.set("FileDescription", "Hoard Setup");
        res.set("CompanyName", "Hoard");
        res.set("LegalCopyright", "AGPL-3.0");
        res.compile()
            .expect("embedding the Windows icon and version resource");
    }
}
