# The Moss Project
The Moss Project (short for Mountable Organised Secure Storage; And Open Source Software!) creates single files that can store files and folders alike and mount them as external drives and such for organised storage that can be shared without compression and then extracted for convenience. Inspired by ISO files, it works similarly, but allows for Writing into it as well.

# Current progress
!!! Worth noting, The windows driver module is mostly AI generated. I will make sure the implementation is functional as intended, but I cannot guarantee there wont be bugs.

~~Functional on linux, being ported to windows for the time being. Have a partially working windows implementation, I estimate the windows shipping wil be done in a day.~~
Both linux and windows builds functioning as intended! I will now whip up a simple ui and a few quality features before public release. To test out the project, dm me on discord @parku9842

When both these are done, the project will officially be released to the public, with precompoled binaries in releases.

# More on Moss
Moss is written in rust and currently only works in cli. as of now, it features attaching files for mounting, inspecting file payload using tui built with ratatui and creating empty .moss files

On linux, it uses Fuse to mount the moss vfs into an existing folder
On windows, it uses Dokany to mount the moss vfs with a drive letter
