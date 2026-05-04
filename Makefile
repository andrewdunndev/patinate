# patinate — convenience targets
#
# `make example`   reproduce the README hero image (3-up gallery)
# `make site`      build the demo site under public/ for GitLab Pages
# `make clean`     remove generated artifacts under assets/ and public/

CARGO ?= cargo
PATINATE = ./target/release/patinate
ASSETS = assets
SITE = public
CONFIG = fixtures/config.toml
OSM = fixtures/grand-rapids.osm.json.gz
ACTIVITIES = fixtures/activities.json
GALLERY_THEMES = noir_heat blueprint_heat warm_beige

.PHONY: all example themes site clean release


themes: $(PATINATE) osm-fixture
	@mkdir -p $(ASSETS)/themes
	@for theme in noir_heat blueprint_heat warm_beige cycle_heat; do \
		echo "render theme preview $$theme"; \
		$(PATINATE) render \
			--config $(CONFIG) \
			--osm $(OSM) \
			--activities $(ACTIVITIES) \
			--theme $$theme \
			--heat-bloom 1.8 --heat-alpha 2.5 \
			--out /tmp/theme-$$theme.svg >/dev/null; \
		rsvg-convert -w 360 /tmp/theme-$$theme.svg \
			-o $(ASSETS)/themes/$$theme.png; \
	done
	@echo "Wrote $(ASSETS)/themes/*.png"

release $(PATINATE):
	$(CARGO) build --release

example: $(PATINATE)
	@mkdir -p $(ASSETS)
	@for theme in $(GALLERY_THEMES); do \
		echo "render $$theme"; \
		$(PATINATE) render \
			--config $(CONFIG) \
			--osm $(OSM) \
			--activities $(ACTIVITIES) \
			--theme $$theme \
			--heat-bloom 1.8 --heat-alpha 2.5 \
			--out /tmp/patinate-example-$$theme.svg >/dev/null; \
		rsvg-convert -w 600 /tmp/patinate-example-$$theme.svg \
			-o /tmp/patinate-example-$$theme.png; \
	done
	rsvg-convert -w 1200 /tmp/patinate-example-noir_heat.svg \
		-o $(ASSETS)/example-noir.png
	# `magick montage` prints a font warning + exits 1 on macOS Homebrew
	# installs that lack a registered Helvetica font, even though the
	# output PNG (no labels requested) is correct. We don't request any
	# labels here, so the warning is cosmetic; tolerate the nonzero exit.
	magick montage \
		/tmp/patinate-example-noir_heat.png \
		/tmp/patinate-example-blueprint_heat.png \
		/tmp/patinate-example-warm_beige.png \
		-tile 3x1 -geometry '+0+0' -background none -gravity center \
		$(ASSETS)/example-gallery.png || true
	@test -s $(ASSETS)/example-gallery.png \
		|| (echo "ERROR: $(ASSETS)/example-gallery.png is missing or empty"; exit 1)
	@echo "Wrote $(ASSETS)/example-noir.png and $(ASSETS)/example-gallery.png"

site: $(PATINATE)
	@mkdir -p $(SITE)
	# Match .gitlab-ci.yml + README "Embed in a page" recipe.
	$(PATINATE) render \
		--config $(CONFIG) \
		--osm $(OSM) \
		--activities $(ACTIVITIES) \
		--theme cycle_heat \
		--web --transparent-bg \
		--out $(SITE)/heatmap-web.svg
	cp examples/site/index.html $(SITE)/index.html
	@echo "Wrote $(SITE)/index.html and $(SITE)/heatmap-web.svg"

clean:
	rm -f $(ASSETS)/example-*.png $(ASSETS)/themes/*.png $(SITE)/index.html $(SITE)/heatmap-web.svg

all: example themes site
