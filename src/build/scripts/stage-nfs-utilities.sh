# {start_index}-{end_index}. NFS server utilities from Alpine packages.
for bin in {nfs_binaries} ; do
  src="$(command -v "$bin")"
  cp "$src" "/out/$bin"
  case "$bin" in
{case_arms}  esac
  echo "[$idx/{total}] $bin OK"
done
