// The six themes the binary embeds, read from the repo's themes/ at
// build time so the site never carries its own copy of an ink set.
import noir_heat from '../../../themes/noir_heat.json';
import blueprint_heat from '../../../themes/blueprint_heat.json';
import warm_beige from '../../../themes/warm_beige.json';
import cycle_heat from '../../../themes/cycle_heat.json';
import newsprint from '../../../themes/newsprint.json';
import verdigris from '../../../themes/verdigris.json';

export interface Road { color: string; width: number }
export interface Theme {
  name: string;
  bg: string;
  text: string;
  water: string;
  parks: string;
  water_line_width?: number;
  road_motorway: Road;
  road_trunk: Road;
  road_primary: Road;
  road_secondary: Road;
  road_tertiary: Road;
  road_residential: Road;
  heat: {
    color: string;
    width: number;
    alpha: number;
    glow_outer_ratio?: number;
    glow_outer_alpha?: number;
    glow_inner_ratio?: number;
    glow_inner_alpha?: number;
    blend?: 'normal' | 'multiply' | 'screen';
  };
  fade_top: { from: string; to: string; height_pct: number };
  fade_bottom: { from: string; to: string; height_pct: number };
}

export interface Edition { theme: Theme; note: string; use: string }

export const editions: Edition[] = [
  { theme: noir_heat as Theme, note: 'Black ground, white road hierarchy, hot orange heat.', use: 'The default poster.' },
  { theme: blueprint_heat as Theme, note: 'Deep blueprint blue, pale roads, amber heat.', use: 'An engineering sheet.' },
  { theme: warm_beige as Theme, note: 'Cream paper, sepia roads, terracotta heat.', use: 'Printing.' },
  { theme: cycle_heat as Theme, note: 'Paper palette, steel-blue heat.', use: 'A web hero on a light page.' },
  { theme: newsprint as Theme, note: 'Letterpress black on gray stock, red heat.', use: 'A print that reads like a page.' },
  { theme: verdigris as Theme, note: 'Bronze roads on a dark green ground, verdigris heat.', use: 'A dark print for a frame.' },
];

/** Bar colors for a theme: ground, two road tiers, water, parks, heat, text. */
export const inks = (t: Theme): string[] =>
  [t.bg, t.road_motorway.color, t.road_primary.color, t.water, t.parks, t.heat.color, t.text];
