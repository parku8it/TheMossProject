# The Moss Project
The Moss Project (short for Mountable Organised Secure Storage; And Open Source Software!) creates single files that can store files and folders alike and mount them as external drives and such for organised storage that can be shared without compression and then extracted for convenience. Inspired by ISO files, it works similarly, but allows for Writing into it as well.

!!!BIG BIG NOTE!!!; Moss files are append only, so files you delete in a moss will just be unlinked, but still stay in the file. Over time this can accumulate and make the file much much bigger than it needs to be. To make your moss big again, just use the clean feature. Personally, it cleaned up to 9GBs of dead data in a few seconds, so its pretty good.

# Current progress
!!! Worth noting, The windows driver module is mostly AI generated. I will make sure the implementation is functional as intended, but I cannot guarantee there wont be bugs.

Both linux and windows builds are now functioning as intended!
Currently, you can only use cli commands or tui to use the program. Feel free to use the code to create your own versions of the program.

I plan to add a native ui using egui or flutter or something.
Also more features, I have one in mind but not sure how to execute it for now and its a bit complex

# More on Moss
Moss is written in rust and currently only works in cli. as of now, it features attaching files for mounting, inspecting file payload using tui built with ratatui and creating empty .moss files

# How to use 
# For Windows:
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

# For Linux:
  Attaching moss to folder:
  ```bat
  \path\to\moss attach \path\to\file.moss \path\to\folder
  ```
  For example when cmd is open in same folder as exe and moss and you want to attach to a child folder named mnt:
  ```bat
  moss attach file.moss ./mnt
  ```
\

  Creating moss in folder:
  ```bat
  \path\to\moss create \desired\path\for\file.moss
  ```
  For example when cmd is open in same folder as exe and you want to create moss there, replace file.moss with the name you want:
  ```bat
  moss create file.moss
  ```
\

  Inspecting moss:
  ```bat
  \path\to\moss inspect \path\to\file.moss
  ```
  For example when cmd is open in same folder as exe and moss:
  ```bat
  moss inspect file.moss
  ```
\

  Cleaning dead data from moss:
  ```bat
  \path\to\moss clean \path\to\file.moss
  ```
  For example when cmd is open in same folder as exe and moss:
  ```bat
  moss clean file.moss
  ```


# Free to use btw do WHATEVER with it i dont care
