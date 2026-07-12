// Placeholder library shown in the hero product shot. The clip names + game grouping
// are slightly ahead of the app (they imply per-clip naming and game auto-detection);
// keep them honest as the app catches up. Swap for real data when available.

export interface Clip {
  title: string;
  meta: string;
  dur: string;
  /** CSS background for the placeholder thumbnail. */
  bg: string;
}

export interface GameGroup {
  game: string;
  /** Accent swatch next to the group heading. */
  color: string;
  clips: Clip[];
}

export interface Library {
  totalLabel: string;
  games: GameGroup[];
}

export const library: Library = {
  totalLabel: '8 clips · 18 MB on disk · 3 games',
  games: [
    {
      game: 'Valorant',
      color: '#ff4655',
      clips: [
        { title: 'Ace on Ascent', meta: 'Jul 4 · 0:18 · 2.6 MB', dur: '0:18', bg: 'radial-gradient(120% 90% at 70% 30%,rgba(255,70,85,.20),#0e0e13 62%)' },
        { title: '1v4 clutch to win the half', meta: 'Jul 4 · 0:22 · 3.1 MB', dur: '0:22', bg: 'radial-gradient(120% 90% at 30% 20%,rgba(0,229,160,.18),#0e0e13 60%)' },
        { title: 'Flick of a lifetime', meta: 'Jul 1 · 0:09 · 1.1 MB', dur: '0:09', bg: 'radial-gradient(120% 90% at 55% 65%,rgba(150,120,255,.18),#0e0e13 62%)' },
      ],
    },
    {
      game: 'Counter-Strike 2',
      color: '#f5b642',
      clips: [
        { title: 'No-scope across mid', meta: 'Jul 3 · 0:07 · 0.7 MB', dur: '0:07', bg: 'radial-gradient(120% 90% at 40% 70%,rgba(64,150,255,.20),#0e0e13 62%)' },
        { title: 'Ninja defuse for the round', meta: 'Jul 1 · 0:15 · 2.2 MB', dur: '0:15', bg: 'radial-gradient(120% 90% at 30% 60%,rgba(0,200,180,.20),#0e0e13 62%)' },
        { title: 'Retake clutch, 1v3', meta: 'Jul 3 · 0:12 · 1.4 MB', dur: '0:12', bg: 'radial-gradient(120% 90% at 60% 40%,rgba(245,182,66,.18),#0e0e13 62%)' },
      ],
    },
    {
      game: 'Overwatch 2',
      color: '#f99e1a',
      clips: [
        { title: 'Reinhardt shatter → team wipe', meta: 'Jul 2 · 0:34 · 5.0 MB', dur: '0:34', bg: 'radial-gradient(120% 90% at 60% 40%,rgba(249,158,26,.18),#0e0e13 62%)' },
        { title: 'Genji blade, quad kill', meta: 'Jul 2 · 0:12 · 1.4 MB', dur: '0:12', bg: 'radial-gradient(120% 90% at 70% 60%,rgba(255,84,112,.16),#0e0e13 62%)' },
      ],
    },
  ],
};
