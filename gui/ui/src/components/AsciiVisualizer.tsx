import type { AsciiVisualizerView } from '../contract';
import { dispatch } from '../ipc';

/** Full-window ASCII audio visualization driven entirely by reducer snapshots. */
export function AsciiVisualizer({ visualizer }: { visualizer: AsciiVisualizerView }) {
	return (
		<section
			aria-label='Audio visualization'
			style={{ zIndex: 45 }}
			className='fixed inset-0 grid grid-rows-[auto_minmax(0,1fr)_auto] overflow-hidden bg-ground text-ink'
		>
			<header className='flex min-w-0 items-center gap-3 border-b border-line px-4 py-2'>
				<h1 className='shrink-0 text-xs font-semibold text-accent'>Audio visualization</h1>
				<span className='shrink-0 font-mono text-xs'>{visualizer.mode}</span>
				<span className='min-w-0 truncate text-xs text-ink-dim'>{visualizer.title}</span>
				<button
					type='button'
					aria-label='Close audio visualization'
					onClick={() => void dispatch('DismissAsciiVisualizer')}
					className='ml-auto rounded border border-line-strong px-2 py-1 text-[11px] text-ink-dim hover:border-ink-faint hover:text-ink'
				>
					Esc
				</button>
			</header>
			<div className='grid min-h-0 place-items-center overflow-hidden p-2'>
				<pre
					aria-label={`${visualizer.mode} audio visualization`}
					className='m-0 max-h-full max-w-full overflow-hidden font-mono text-[clamp(5px,0.86vw,14px)] leading-none tracking-normal text-ink whitespace-pre'
				>
					{visualizer.lines.join('\n')}
				</pre>
			</div>
			<footer className='flex items-center justify-center gap-3 border-t border-line px-4 py-2 text-[11px] text-ink-faint'>
				<button
					type='button'
					onClick={() => void dispatch('PreviousAsciiVisualization')}
					className='rounded border border-line-strong px-2 py-1 hover:border-ink-faint hover:text-ink'
				>
					Left
				</button>
				<span>switch visualization</span>
				<button
					type='button'
					onClick={() => void dispatch('NextAsciiVisualization')}
					className='rounded border border-line-strong px-2 py-1 hover:border-ink-faint hover:text-ink'
				>
					Right
				</button>
			</footer>
		</section>
	);
}
