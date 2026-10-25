# The i686 Windows build environment for the profiler ASI.
#
# The Tracy client is C++, and Rust's prebuilt i686-pc-windows-gnu std unwinds with DWARF-2
# (`_Unwind_Resume`). A MinGW whose C++ runtime uses SJLJ (`_Unwind_SjLj_*`, e.g. Homebrew's) cannot
# link the two. Debian's mingw-w64 i686 posix toolchain is built `--disable-sjlj-exceptions
# --with-dwarf2` and defaults to msvcrt, matching Rust's target.
FROM rust:1.94.1-slim-trixie

RUN apt-get update \
 && apt-get install -y --no-install-recommends g++-mingw-w64-i686-posix \
 && rm -rf /var/lib/apt/lists/* \
 && update-alternatives --set i686-w64-mingw32-gcc /usr/bin/i686-w64-mingw32-gcc-posix \
 && update-alternatives --set i686-w64-mingw32-g++ /usr/bin/i686-w64-mingw32-g++-posix \
 && rustup target add i686-pc-windows-gnu

# rustc links `-lstdc++` after `-Bdynamic`, which picks libstdc++.dll.a and makes the ASI import
# libstdc++-6.dll (absent from the game folder). A directory holding only the static archive, searched
# first, links libstdc++ into the ASI.
RUN mkdir -p /opt/static-cxx \
 && ln -s "$(dirname "$(i686-w64-mingw32-g++ -print-libgcc-file-name)")/libstdc++.a" /opt/static-cxx/libstdc++.a \
 && test -f /opt/static-cxx/libstdc++.a
ENV CARGO_TARGET_I686_PC_WINDOWS_GNU_LINKER=i686-w64-mingw32-gcc
ENV CARGO_TARGET_I686_PC_WINDOWS_GNU_RUSTFLAGS="-L native=/opt/static-cxx"
