import React from 'react';
import {CalculateMetadataFunction, Composition, Freeze, staticFile, Still} from 'remotion';
import {Demo, DemoProps} from './Demo';
import {plan, Timeline} from './schedule';

const WIDTH = 1920;
const HEIGHT = 1200;

// The recording's timeline, read once before the render (script/demo-video writes it).
const readTimeline: CalculateMetadataFunction<DemoProps> = async ({props}) => {
  const response = await fetch(staticFile('film/timeline.json'));
  if (!response.ok) {
    throw new Error('No recording in demo/public/film: run script/demo-video first');
  }
  const timeline = (await response.json()) as Timeline;
  return {
    props: {...props, timeline},
    fps: timeline.fps,
    durationInFrames: plan(timeline).durationInFrames,
  };
};

/** The README's poster: the film at its `mark:poster`. */
const Poster: React.FC<DemoProps> = (props) => (
  <Freeze frame={props.timeline ? plan(props.timeline).poster : 0}>
    <Demo {...props} />
  </Freeze>
);

export const Root: React.FC = () => (
  <>
    <Composition
      id="Demo"
      component={Demo}
      width={WIDTH}
      height={HEIGHT}
      fps={60}
      durationInFrames={60}
      defaultProps={{timeline: null}}
      calculateMetadata={readTimeline}
    />
    <Still
      id="Poster"
      component={Poster}
      width={WIDTH}
      height={HEIGHT}
      defaultProps={{timeline: null}}
      calculateMetadata={readTimeline}
    />
  </>
);
