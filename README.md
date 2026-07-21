# The Moss Project
The Moss Project (short for Mountable Organised Secure Storage; And Open Source Software!) creates single files that can store files and folders alike and mount them as external drives and such for organised storage that can be shared without compression and then extracted for convenience. Inspired by ISO files, it works similarly, but allows for Writing into it as well.

!!!BIG BIG NOTE!!!; Moss files are append only, so files you delete in a moss will just be unlinked, but still stay in the file. Over time this can accumulate and make the file much much bigger than it needs to be. To make your moss big again, just use the clean feature. Personally, it cleaned up to 9GBs of dead data in a few seconds, so its pretty good.

# Current progress
!!! Worth noting, The windows driver module is mostly AI generated. I will make sure the implementation is functional as intended, but I cannot guarantee there wont be bugs.

Both linux and windows builds are now functioning as intended!
Currently, you can only use cli commands or tui to use the program. Feel free to use the code to create your own versions of the program.

# Goals
I plan to add a native ui using egui or flutter or something.
Also more features, I have one in mind but not sure how to execute it for now and its a bit complex

## Android
I am considering expanding support to android, but I cannot promise it will ever go out to the public. If you need it that bad, just use termux with file storage command and rust compilation packages then build a binary using `cargo build` with optional `--release` tag, find the bin in target/debug or target/release. on the first ever run, it took my old phone only a few minutes. other builds will generally only take a few dozen seconds if you didnt mess with dependencies.

## MacOS/OSX, iOS, iPadOS
No current plans to ship to this even though I have an ipad. I dont own a macbook and am not planning to hackintosh any time soon (my mind may change, I've done hackintosh before and still have an EFI folder ready. I just dont need it, yet.)

# More on Moss
Moss is written in rust and currently only works in cli. as of now, it features attaching files for mounting, inspecting file payload using tui built with ratatui and creating empty .moss files

# How to use 
!!!BIG BIG NOTE (yes, again)!!!; Moss files are append only, so files you delete in a moss will just be unlinked, but still stay in the file. Over time this can accumulate and make the file much much bigger than it needs to be. To make your moss big again, just use the clean feature. Personally, it cleaned up to 9GBs of dead data in a few seconds, so its pretty good.

## For Windows:
  Attaching moss to drive letter:
  ```bat
  \path\to\moss.exe attach \path\to\file.moss {Drive Letter}:
  ```
  For example when cmd is open in same folder as exe and moss and you want to attach to Z drive:
  ```bat
  moss.exe attach file.moss Z:
  ```
\

  Creating moss in folder:
  ```bat
  \path\to\moss.exe create \desired\path\for\file.moss
  ```
  For example when cmd is open in same folder as exe and you want to create moss there, replace file.moss with the name you want:
  ```bat
  moss.exe create file.moss
  ```
\

  Inspecting moss:
  ```bat
  \path\to\moss.exe inspect \path\to\file.moss
  ```
  For example when cmd is open in same folder as exe and moss:
  ```bat
  moss.exe inspect file.moss
  ```
\

  Cleaning dead data from moss:
  ```bat
  \path\to\moss.exe clean \path\to\file.moss
  ```
  For example when cmd is open in same folder as exe and moss:
  ```bat
  moss.exe clean file.moss
  ```
\

## For Linux:
  Attaching moss to folder:
  ```bat
  /path/to/moss attach /path/to/file.moss /path/to/folder
  ```
  For example when cmd is open in same folder as exe and moss and you want to attach to a child folder named mnt:
  ```bat
  moss attach file.moss ./mnt
  ```
\

  Creating moss in folder:
  ```bat
  /path/to/moss create /desired/path/for/file.moss
  ```
  For example when cmd is open in same folder as exe and you want to create moss there, replace file.moss with the name you want:
  ```bat
  moss create file.moss
  ```
\

  Inspecting moss:
  ```bat
  /path/to/moss inspect /path/to/file.moss
  ```
  For example when cmd is open in same folder as exe and moss:
  ```bat
  moss inspect file.moss
  ```
\

  Cleaning dead data from moss:
  ```bat
  /path/to/moss clean /path/to/file.moss
  ```
  For example when cmd is open in same folder as exe and moss:
  ```bat
  moss clean file.moss
  ```

# Building from source

## For Linux:

First get rust. You can use packages like rust from the Arch official repo but I suggest rustup because it will let you cross compile in case you need it; Install it from https://rustup.rs if you dont have it.

```bash
git clone https://github.com/parku8it/TheMossProject.git
cd moss
cargo build --release
```

If it compiles successfully, u can find the bin at `target/release/moss`

If you get fuse errors, you probably need libfuse or fuse3 installed:

```bash
# Debian/Ubuntu
sudo apt install fuse3 libfuse3-dev

# Fedora
sudo dnf install fuse3 fuse3-devel

# Arch
sudo pacman -S fuse3
```

## For Windows:

get rust from https://rustup.rs. (NOT chocolatey, that one sucks lol and) because we need the msvc toolchain which rustup should set up automatically.

also get Dokan installed on your system to mount moss files on windows; get it from https://github.com/dokan-dev/dokany/releases (install the x64 or arm64 msi depending on your system, or also just get the dokan setup.exe).

Then

```bat
git clone https://github.com/parku8it/TheMossProject.git
cd moss
cargo build --release
```

The bin will be at `target\release\moss.exe` just like linux

If cargo complains about linker stuff, install Visual Studio Build Tools or just use the "Desktop development with C++" thing from Visual Studio Installer. Dont get gnueabi even if ai tells u to, just use msvc. I dont think dokany works on gnu.
Or better, just use the builds I provide in releases if you're not using a custom version.

## Pre-built binaries

I provide builds for both windows and linux, both for arm and x64. I may or may not provide for android if i ever end up finishing an android version.

# Versioning
The program also increases version every build, so I can know if I uploaded the right version here or for my friends.
I did this using with the build.rs in project root.

# Free to use btw do WHATEVER with it i dont care
