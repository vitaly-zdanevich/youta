import { useState } from "react";

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
  className,
  onClick,
}: {
  url: string | null | undefined;
  className: string;
  onClick?: () => void;
}) {
  const [failed, setFailed] = useState(false);
  const source = artworkSource(url);

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
      onError={() => setFailed(true)}
    />
  );
}
