# {{ start_index }}-{{ end_index }}. k3s host utilities from Alpine packages.
for bin in {{ utility_packages }} ; do
  src="$(command -v "$bin")"
  cp "$src" "/out/$bin"
  case "$bin" in
{{ case_arms }}  esac
  echo "[$idx/{{ total }}] $bin OK"
done
