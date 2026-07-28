# Copyright 2026 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

EAPI=8

# Release builds use the upstream cargo-vendor archive rather than hundreds of
# individual crate distfiles. Cargo.lock remains authoritative.
CRATES=""
RUST_MIN_VER="1.95.0"

inherit cargo

DESCRIPTION="Low-resource YouTube audio player TUI with local subscriptions and progress"
HOMEPAGE="https://github.com/vitaly-zdanevich/youta"
SRC_URI="
	https://github.com/vitaly-zdanevich/youta/archive/refs/tags/v${PV}.tar.gz
		-> ${P}.tar.gz
	https://github.com/vitaly-zdanevich/youta/releases/download/v${PV}/${P}-vendor.tar.xz
"

# Youta itself is MIT. The remaining licenses cover the locked Rust dependency
# graph and the optional public-domain SQLite amalgamation shipped upstream.
LICENSE="
	0BSD Apache-2.0 Apache-2.0-with-LLVM-exceptions BSD Boost-1.0 CC0-1.0
	CDLA-Permissive-2.0 ISC LGPL-2.1+ MIT MPL-2.0 Unicode-3.0
	Unicode-DFS-2016 Unlicense ZLIB public-domain
"
SLOT="0"
KEYWORDS="~amd64 ~arm64"

IUSE="
	+alsa archive-rar +archive-zip archive-org apple-podcasts +bbc-radio
	bandcamp bilibili bundled-sqlite dearrow discord evernote
	+funkwhale +generic-ytdlp google-drive +gpm gpodder +invidious jack +jamendo keyring
	lastfm librivox +litres +local +mpv native odysee +peertube pipewire
	podcast-index pulseaudio +radio +rss rumble +rutube +soundcloud +soundstream
	sponsorblock sqlite ssh telegram test +thumbnails torrent +tracker-music +tui +vimeo vk
	waveform webdav wikidata wikimedia yandex-disk yandex-music +youtube-music
	+youtube-official +yt-dlp
"

REQUIRED_USE="
	apple-podcasts? ( rss )
	bandcamp? ( yt-dlp )
	bbc-radio? ( radio )
	bundled-sqlite? ( sqlite )
	gpodder? ( rss )
	generic-ytdlp? ( yt-dlp )
	gpm? ( tui )
	podcast-index? ( rss )
	rutube? ( yt-dlp )
	soundcloud? ( yt-dlp )
	tracker-music? ( archive-zip mpv )
	vimeo? ( yt-dlp )
	youtube-music? ( yt-dlp )
"
RESTRICT="!test? ( test )"

RDEPEND="
	sqlite? ( !bundled-sqlite? ( dev-db/sqlite:3 ) )
	archive-rar? ( app-arch/unrar )
	archive-zip? ( app-arch/unzip )
	keyring? ( app-crypt/libsecret )
	mpv? ( >=media-video/mpv-0.38[alsa?,cli,jack?,pipewire?,pulseaudio?] )
	tracker-music? (
		media-libs/libopenmpt
		media-video/ffmpeg[openmpt]
	)
	yt-dlp? (
		media-video/ffmpeg
		net-misc/yt-dlp
	)
"
DEPEND="${RDEPEND}"

# package-vendor.sh lays the vendor tree beneath the same ${P} directory as the
# GitHub source archive. cargo.eclass then creates an offline CARGO_HOME that
# points at this exact tree.
ECARGO_VENDOR="${S}/vendor"

src_unpack() {
	cargo_src_unpack
}

src_prepare() {
	default

	[[ -d ${ECARGO_VENDOR} ]] ||
		die "The upstream vendor archive did not contain ${ECARGO_VENDOR}"
	[[ -f Cargo.lock ]] || die "Cargo.lock is required for an offline build"
}

src_configure() {
	local myfeatures=(
		$(usev alsa)
		$(usev archive-rar)
		$(usev archive-zip)
		$(usev archive-org)
		$(usev apple-podcasts)
		$(usev bandcamp)
		$(usev bbc-radio)
		$(usev bilibili)
		$(usev bundled-sqlite)
		$(usev dearrow)
		$(usev discord)
		$(usev evernote)
		$(usev funkwhale)
		$(usev generic-ytdlp)
		$(usev google-drive)
		$(usev gpm)
		$(usev gpodder)
		$(usev invidious)
		$(usev jack)
		$(usev jamendo)
		$(usev keyring)
		$(usev lastfm)
		$(usev librivox)
		$(usev litres)
		$(usev local)
		$(usev odysee)
		$(usev peertube)
		$(usev pipewire)
		$(usev podcast-index)
		$(usev pulseaudio)
		$(usev radio)
		$(usev rss)
		$(usev rumble)
		$(usev rutube)
		$(usev soundcloud)
		$(usev soundstream)
		$(usev sponsorblock)
		$(usev sqlite sqlite-state)
		$(usev ssh)
		$(usev telegram)
		$(usev thumbnails)
		$(usev torrent)
		$(usev tracker-music)
		$(usev tui)
		$(usev vimeo)
		$(usev vk)
		$(usev waveform)
		$(usev webdav)
		$(usev wikidata)
		$(usev wikimedia)
		$(usev yandex-disk)
		$(usev yandex-music)
		$(usev youtube-music)
		$(usev youtube-official)
		$(usev yt-dlp)
	)

	use mpv && myfeatures+=( backend-mpv )
	use native && myfeatures+=( backend-native )

	cargo_src_configure --locked --no-default-features
}

src_install() {
	cargo_src_install

	dodoc README.md config.example.toml
	dodoc docs/ARCHITECTURE.md docs/AUDIOPHILE.md docs/FEASIBILITY.md
}

pkg_postinst() {
	if use yt-dlp; then
		elog "yt-dlp support is opt-in at runtime and follows provider terms."
		elog "Keep net-misc/yt-dlp current because site extractors change."
	fi

	if use alsa && use mpv; then
		elog "List ALSA devices with: mpv --audio-device=help"
	fi

	if use gpm; then
		elog "GPM mouse input is used opportunistically on /dev/ttyN."
		elog "No GPM daemon is required; F8 enables Youta's keyboard pointer."
	fi
}
