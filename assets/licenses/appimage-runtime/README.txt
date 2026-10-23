The AppImage runtime

Every AppImage starts with a small program, the runtime, that mounts the rest of the file and
starts Gumka from it. Gumka's AppImage uses the type 2 runtime, release 20251108, from
https://github.com/AppImage/type2-runtime (MIT, type2-runtime.txt). It is built in Alpine Linux
and linked statically with these libraries, whose licenses are in this folder:

- libfuse 3.15.0, https://github.com/libfuse/libfuse: GNU LGPL 2.1 (libfuse-LGPL2.txt). The
  runtime's source and build scripts, with the patch it applies to libfuse, are in the
  repository above.
- squashfuse 0.5.2, https://github.com/vasi/squashfuse: BSD 2-Clause (squashfuse.txt)
- zstd, https://github.com/facebook/zstd: BSD 3-Clause (zstd.txt)
- zlib, https://zlib.net: zlib license (zlib.txt)
- mimalloc, https://github.com/microsoft/mimalloc: MIT (mimalloc.txt)
- musl libc, https://musl.libc.org: MIT (musl.txt)
