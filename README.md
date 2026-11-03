<img src="assets/icon.png" width="96" alt="Amber widget icon">

# Amber widget

Linux desktop dashboard and tray icon for Amber Electric prices and grid usage.

Build with `cargo build --release`, or download the binary from the latest `build` workflow run.

To get the icon in your launcher and alt-tab on every desktop (Wayland needs this):

```bash
install -D target/release/amber-widget ~/.local/bin/amber-widget
install -D assets/icon.png ~/.local/share/icons/hicolor/256x256/apps/amber-widget.png
install -D assets/amber-widget.desktop ~/.local/share/applications/amber-widget.desktop
```
