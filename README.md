# download-assistant 

Downloads a list of files described in a CSV, creates the required directory structure, and automatically extracts archives.
![rust](https://img.shields.io/badge/rust-2021-vert?logo=rust)

# Transcribe


![License](https://img.shields.io/badge/license-MIT-green)
![Rust](https://img.shields.io/badge/python-3.12-blue)



## Compilation

```bash
cargo build --release
# → target/release/install-assistant
```
No system dependencies required: TLS, bzip2, and liblzma are statically compiled into the binary.

## Utilisation

```
install-assistant [fichier.csv] [répertoire_base]
```
| Argument         | Default           | Description           |
| ---------------- | ----------------- | --------------------- |
| `csv_file`       | `downloads.csv`   | List of files         |
| `base_directory` | Current directory | Root destination path |

## Format du CSV

```
destination,url[,format]
```

| Column | Required | Description                                          |
| ------ | -------- | ---------------------------------------------------- |
| 1      | ✅        | Destination directory (relative to `base_directory`) |
| 2      | ✅        | File URL (`http://` or `https://`)                   |
| 3      | ❌        | Extraction format (see table below)                  |


### Valeurs de la colonne format

| Value                         | Behavior                           |
| ----------------------------- | ---------------------------------- |
| *(missing or empty)* / `auto` | Detects format from file extension |
| `zip`                         | ZIP extraction                     |
| `7z` / `7zip`                 | 7-Zip extraction                   |
| `tar.gz` / `tgz`              | TAR + GZIP extraction              |
| `tar.bz2` / `tbz2`            | TAR + BZIP2 extraction             |
| `tar.xz` / `txz`              | TAR + XZ extraction                |
| `gz`                          | Decompress single `.gz` file       |
| `no`                          | No extraction (even if archive)    |


### Full example

```csv
# Whisper models (raw .bin files, no extraction)
build/whisper/models,https://huggingface.co/.../ggml-base.bin

# ZIP archive → extracted into tools/ffmpeg/
tools/ffmpeg,https://example.com/ffmpeg-linux.zip,zip

# ZIP archive → extracted into build/dic
build/dic,https://github.com/fhoudebert/transcribe/releases/download/v1.0/dic-fr.zip,zip

# tar.gz archive → extracted into build/ffmpeg/bin
build/ffmpeg/bin,https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-linux64-lgpl.tar.xz,tgz

# Auto-detected .tar.xz → tar+xz extraction
tools/cmake,https://example.com/cmake-3.30.tar.xz

# Keep ZIP as-is (no extraction)
dist,https://example.com/release.zip,no
```

## Guaranteed behavior

| Situation                    | Behavior                                   |
| ---------------------------- | ------------------------------------------ |
| Missing directory            | Automatically created                      |
| File already exists          | Skipped (idempotent)                       |
| Interrupted download         | `.tmp` file cleaned, destination untouched |
| HTTP error                   | Error logged, next entry processed         |
| CDN redirects (HuggingFace…) | Automatically followed (max 15 redirects)  |
| Exit code                    | `0` = success · `1` = at least one error   |


## Supported formats

| Format               | Rust crate        | System dependency             |
| -------------------- | ----------------- | ----------------------------- |
| `.zip`               | `zip 2`           | None                          |
| `.7z`                | `sevenz-rust 0.6` | None                          |
| `.tar.gz` / `.tgz`   | `tar` + `flate2`  | None                          |
| `.tar.bz2` / `.tbz2` | `tar` + `bzip2`   | libbz2 (statically compiled)  |
| `.tar.xz` / `.txz`   | `tar` + `xz2`     | liblzma (statically compiled) |
| `.gz` (single file)  | `flate2`          | None                          |




