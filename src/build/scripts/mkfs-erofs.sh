apk add --no-cache erofs-utils >/dev/null && \
  cp -a /rootfs /build && \
  mknod -m 600 /build/dev/console c 5 1 && \
  mknod -m 666 /build/dev/null c 1 3 && \
  exec mkfs.erofs "$@"
