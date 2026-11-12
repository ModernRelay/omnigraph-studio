import React from 'react';
import {AbsoluteFill} from 'remotion';

// The desktop the window sits on: our own design, soft colour fields over a pale ground, drawn
// with CSS gradients so it needs no image and scales with the camera. (Never a system
// wallpaper: those are not ours to use.)

export const Backdrop: React.FC = () => (
  <AbsoluteFill
    style={{
      backgroundColor: '#e8ebf4',
      backgroundImage: [
        'radial-gradient(60% 70% at 12% 18%, rgba(122, 140, 255, 0.55), transparent 70%)',
        'radial-gradient(55% 65% at 88% 12%, rgba(196, 150, 255, 0.45), transparent 70%)',
        'radial-gradient(70% 70% at 80% 95%, rgba(255, 176, 140, 0.50), transparent 70%)',
        'radial-gradient(60% 60% at 10% 92%, rgba(110, 210, 200, 0.40), transparent 70%)',
        'linear-gradient(160deg, #eef0f8 0%, #e2e6f2 100%)',
      ].join(', '),
    }}
  />
);

