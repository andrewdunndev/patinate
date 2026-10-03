# patinate: convenience recipes. `just --list` shows them all.

patinate := "./target/release/patinate"
config := "fixtures/config.toml"
osm := "fixtures/grand-rapids.osm.json.gz"
activities := "fixtures/activities.json"
scratch := "target/render"
# The committed assets render from fixtures/config.toml alone. config::load
# merges $XDG_CONFIG_HOME/patinate/config.toml and PATINATE_*__* over it,
# which would carry a real home and salt into public files, so each render
# reads an empty config dir and _fixture-env refuses the env overrides.
# Scoped to patinate: a global export would hide mise's own config.
no_user_config := justfile_directory() / "target/no-user-config"
render := "XDG_CONFIG_HOME=" + quote(no_user_config) + " " + patinate + " render --config " + config + " --osm " + osm + " --activities " + activities

_default:
    @just --list

# reproduce the README hero image, theme previews and demo site
all: example themes site

# build the optimized binary at target/release/patinate
release:
    cargo build --release

# fail when a PATINATE_*__* config override is exported
_fixture-env:
    #!/usr/bin/env bash
    set -euo pipefail
    if names=$(compgen -e | grep -E '^PATINATE_[A-Z0-9_]*__'); then
        echo "unset before rendering fixtures:" $names >&2
        exit 1
    fi

# reproduce the README hero image (3-up gallery)
example: _fixture-env release
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p assets {{scratch}}
    for theme in noir_heat blueprint_heat warm_beige; do
        echo "render $theme"
        {{render}} --theme "$theme" --heat-bloom 1.8 --heat-alpha 2.5 \
            --out "{{scratch}}/example-$theme.svg" >/dev/null
        rsvg-convert -w 600 "{{scratch}}/example-$theme.svg" -o "{{scratch}}/example-$theme.png"
    done
    rsvg-convert -w 1200 {{scratch}}/example-noir_heat.svg -o assets/example-noir.png
    # magick montage exits 1 over a missing Helvetica on Homebrew even
    # though the label-less output is correct; check the file instead.
    magick montage {{scratch}}/example-{noir_heat,blueprint_heat,warm_beige}.png \
        -tile 3x1 -geometry '+0+0' -background none -gravity center \
        assets/example-gallery.png || true
    test -s assets/example-gallery.png
    echo "Wrote assets/example-noir.png and assets/example-gallery.png"

# render the four theme previews into assets/themes
themes: _fixture-env release
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p assets/themes {{scratch}}
    for theme in noir_heat blueprint_heat warm_beige cycle_heat; do
        echo "render theme preview $theme"
        {{render}} --theme "$theme" --heat-bloom 1.8 --heat-alpha 2.5 \
            --out "{{scratch}}/theme-$theme.svg" >/dev/null
        rsvg-convert -w 360 "{{scratch}}/theme-$theme.svg" -o "assets/themes/$theme.png"
    done
    echo "Wrote assets/themes/*.png"

# build public/ for a local preview of the README embed recipe
site: _fixture-env release
    mkdir -p public
    {{render}} --theme cycle_heat --web --transparent-bg --out public/heatmap-web.svg
    cp examples/site/index.html public/index.html

# refresh the committed heatmap and images that web/ serves
web: _fixture-env example themes
    #!/usr/bin/env bash
    set -euo pipefail
    {{render}} --theme cycle_heat --web --transparent-bg --out web/public/heatmap-web.svg
    cp assets/example-noir.png assets/example-gallery.png web/public/
    for f in assets/themes/*.png; do cp "$f" "web/public/theme-$(basename "$f")"; done

# run the rasterizing tests (need rsvg-convert on PATH)
check-pixels:
    cargo test -- --ignored

# remove generated artifacts under assets/ and public/
clean:
    rm -f assets/example-*.png assets/themes/*.png public/index.html public/heatmap-web.svg
