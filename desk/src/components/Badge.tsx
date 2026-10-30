import type { ReactNode } from 'react';

import type { Tone } from '../status';

interface Props {
  tone?: Tone;
  className?: string;
  children: ReactNode;
}

/** A small label. The text carries the meaning; the colour only supports it. */
export function Badge({ tone = 'neutral', className, children }: Props) {
  return <span className={`badge badge-${tone}${className ? ` ${className}` : ''}`}>{children}</span>;
}
