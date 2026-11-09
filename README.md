# brrr

Printing on Linux without CUPS, drivers, or a daemon. It finds the printer and prints.

```
brrr report.pdf
brrr photo.jpg --size 4x6
brrr notes.txt -2 -n 3 -p 1-4
brrr essay.docx
brrr list | status | jobs | cancel 12 | default "Office" | doctor
```

## Install

```
cargo build --release
sudo install target/release/brrr /usr/local/bin/
sudo pacman -S poppler pango       # PDFs and non-latin text
sudo pacman -S ipp-usb             # only for USB printers
sudo pacman -S libreoffice-fresh   # only for .docx and friends
```

## Make Ctrl+P use it

```
brrr serve --install
```

This runs a small per-user service on 127.0.0.1:6631 and points `~/.cups/client.conf` at it, so GTK, Qt and other libcups apps print through brrr. It doesn't need root and doesn't touch a system cupsd. Any existing client.conf is backed up. `brrr serve --uninstall` restores it.

On Arch, GTK dialogs also need `gtk3-print-backends`. Sandboxed Flatpak apps have their own CUPS config and won't pick this up.

Optional: `ln -s brrr lp` and `ln -s brrr lpr` so scripts that call those land here.

## How it works

1. Finds printers with mDNS (`_ipp._tcp`, `_ipps._tcp`), plus ipp-usb on localhost:60000-60003.
2. Asks the printer what it supports over IPP.
3. Sends the file as-is if the printer takes it. Otherwise renders it to PWG Raster or Apple Raster (URF).
4. Follows the job and reports problems in plain words ("out of paper", not `media-empty-error`).

If `--pages` or copies aren't supported by the printer, brrr handles them on its side.

## Encryption

brrr uses `ipps` whenever a printer offers it. Printers use self-signed certificates, so brrr remembers each one the first time it connects (`~/.config/brrr/known_printers`) and refuses to send anything if it changes. After a printer reset, run `brrr forget PRINTER`.

If a printer also offers plain IPP and its TLS has never worked, brrr warns and uses plain. It never downgrades a printer it has already pinned.

## Tested

Run against CUPS's `ippeveprinter` reference printer, with its output decoded by libcups:

- PWG raster in sRGB, gray, and 1-bit dithered; URF in sRGB and gray
- duplex back-side transforms, page ranges, copies, transparency, landscape rotation
- PDF and JPEG passthrough, busy retry, mDNS discovery through a real Avahi daemon
- `brrr serve` with real `lp` and `lpstat`, and a real GTK print job (duplex, copies, gray, paper size). Generated PPDs pass `cupstestppd`.
- TLS and certificate-change refusal, using a test TLS proxy in front of the reference printer
- .docx through LibreOffice, PostScript, CJK, Arabic, Cyrillic, and emoji text
- `cargo test`: wire parsing, malformed packets, compression round-trips

**Not yet tested on a physical printer, or with a Qt print dialog clicked through by hand.**

## Not done yet

- No printer logins.
- Discovery is IPv4 only.
- N-up (several pages per sheet) from app dialogs isn't offered.
