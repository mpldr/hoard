fn main() {
    slint_build::compile("ui/installer.slint").expect("compiling the installer UI");

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
