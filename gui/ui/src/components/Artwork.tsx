import { useEffect, useState } from 'react';

import { artworkSource } from "../ipc";

/**
 * Artwork served by Rust over the `youta://` protocol.
 *
 * Unavailable artwork leaves a tidy placeholder rather than a broken icon,
 * because a refusal here is ordinary: the source may be private, unreachable,
 * or not an image at all, and Rust declines all three identically.
 */
export function Artwork({
  url,
	prefetchUrl,
  className,
  onClick,
}: {
  url: string | null | undefined;
	/** Selected Details only: warm this rendition after the visible image loads. */
	prefetchUrl?: string | null;
  className: string;
  onClick?: () => void;
}) {
  const [failed, setFailed] = useState(false);
	const [loadedSource, setLoadedSource] = useState<string | null>(null);
  const source = artworkSource(url);
	const prefetchSource = artworkSource(prefetchUrl);

	useEffect(() => {
		if (!source || loadedSource !== source || !prefetchSource || prefetchSource === source) return;
		// One selected owner warms Rust's guarded cache and the browser's decode
		// cache. Rows never supply prefetchUrl, and a selection change retires it.
		let current = true;
		const image = new Image();
		image.decoding = 'async';
		image.onload = () => {
			if (current) void image.decode().catch(() => {});
		};
		image.onerror = () => {};
		image.src = prefetchSource;
		return () => {
			current = false;
			image.onload = null;
			image.onerror = null;
			image.removeAttribute('src');
		};
	}, [source, loadedSource, prefetchSource]);

  if (source === null || failed) {
    return <div className={`${className} bg-raised`} aria-hidden="true" />;
  }
  return (
    <img
      className={onClick ? `${className} cursor-zoom-in` : className}
      src={source}
      alt=""
      loading="lazy"
      decoding="async"
      onClick={onClick}
		onLoad={() => { if (prefetchSource) setLoadedSource(source); }}
      onError={() => setFailed(true)}
    />
  );
}
