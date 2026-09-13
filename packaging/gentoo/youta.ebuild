# Copyright 2026 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

# Unpublished next-release template; see packaging/gentoo/README.md.
# Copy to a new versioned overlay filename only after its distfiles are published.
EAPI=8

# Tagged releases use the upstream cargo-vendor archive rather than hundreds of
# individual crate distfiles. Cargo.lock remains authoritative.
CRATES=""
RUST_MIN_VER="1.95.0"

inherit cargo

DESCRIPTION="Low-resource media player with terminal and optional desktop interfaces"
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
KEYWORDS="~amd64 ~arm64 ~x86"

IUSE="
	+acoustid +alsa +ascii-visualizer +audio-quality archive-rar +archive-zip +archive-org +archive-upload apple-podcasts +bbc-radio
	bandcamp bilibili bundled-sqlite cpu_flags_x86_sse2 dearrow discord +evernote
	+commons-upload +funkwhale +generic-ytdlp google-drive gpm gpodder gui +invidious jack +jamendo
	keyring +lan-sharing +lastfm +librivox +litres +local +local-archives +mpv native +nyan-cat odysee +peertube pipewire
	podcast-index pulseaudio +qr +radio +rss rumble +rutube +soundcloud +soundstream
	+sponsorblock sqlite ssh +summary telegram test +images torrent +tracker-music +tui +vimeo vk
	+waveform +web-browser webdav +wikidata wikimedia yandex-disk yandex-music +youtube-music
	+youtube-captions +youtube-official +yt-dlp
"

REQUIRED_USE="
	acoustid? ( local )
	ascii-visualizer? ( mpv )
	audio-quality? ( local )
	apple-podcasts? ( rss )
	bandcamp? ( yt-dlp )
	bbc-radio? ( radio )
	bundled-sqlite? ( sqlite )
	archive-upload? ( yt-dlp )
	commons-upload? ( yt-dlp )
	evernote? ( yt-dlp )
	gpodder? ( rss )
	generic-ytdlp? ( yt-dlp )
	gpm? ( tui )
	gui? ( tui )
	images? ( tui )
	lan-sharing? ( local qr yt-dlp )
	lastfm? ( acoustid wikidata )
	local-archives? ( archive-zip local )
	podcast-index? ( rss )
	qr? ( tui )
	rutube? ( yt-dlp )
	soundcloud? ( yt-dlp )
	summary? ( yt-dlp )
	tracker-music? ( archive-zip mpv )
	vimeo? ( yt-dlp )
	wikimedia? ( commons-upload )
	youtube-music? ( yt-dlp )
	youtube-captions? ( yt-dlp )
	x86? ( cpu_flags_x86_sse2 )
"
RESTRICT="!test? ( test )"

RDEPEND="
	!media-sound/youta-bin
	acoustid? ( media-libs/chromaprint[tools] )
	ascii-visualizer? ( media-sound/cava )
	audio-quality? ( media-video/ffmpeg )
	sqlite? ( !bundled-sqlite? ( dev-db/sqlite:3 ) )
	archive-rar? ( app-arch/unrar )
	local-archives? ( app-arch/unrar )
	local? (
		images? ( media-video/ffmpeg )
	)
	gpm? ( sys-libs/gpm )
	archive-zip? ( app-arch/unzip )
	gui? (
		app-arch/unzip
		dev-libs/libayatana-appindicator
		media-libs/chromaprint[tools]
		media-libs/libopenmpt
		media-video/ffmpeg[openmpt]
		>=media-video/mpv-0.38[alsa?,cli,jack?,pipewire?,pulseaudio?]
		>=net-libs/webkit-gtk-2.40:4.1
		x86? (
			>=dev-libs/quickjs-ng-0.12.0
			net-misc/yt-dlp[-deno]
		)
		!x86? ( net-misc/yt-dlp )
		sys-apps/dbus
		x11-libs/gtk+:3
	)
	keyring? ( app-crypt/libsecret )
	mpv? ( >=media-video/mpv-0.38[alsa?,cli,jack?,pipewire?,pulseaudio?] )
	tracker-music? (
		media-libs/libopenmpt
		media-video/ffmpeg[openmpt]
	)
	waveform? ( media-video/ffmpeg )
	yt-dlp? (
		media-video/ffmpeg
		x86? (
			>=dev-libs/quickjs-ng-0.12.0
			net-misc/yt-dlp[-deno]
		)
		!x86? ( net-misc/yt-dlp )
	)
"
DEPEND="${RDEPEND}"
BDEPEND="
	gui? ( virtual/pkgconfig )
	sqlite? ( !bundled-sqlite? ( virtual/pkgconfig ) )
"

# package-vendor.sh lays the vendor tree and production GUI page beneath the
# same ${P} directory as the GitHub source archive. cargo.eclass then creates
# an offline CARGO_HOME that points at this exact tree.
ECARGO_VENDOR="${S}/vendor"

src_unpack() {
	cargo_src_unpack
}

src_prepare() {
	default

	# cargo.eclass generates an absolute offline mapping for ECARGO_VENDOR.
	# Cargo rejects upstream's second source name for the same directory.
	rm -f .cargo/config.toml ||
		die "failed to remove the duplicate upstream Cargo source mapping"

	[[ -d ${ECARGO_VENDOR} ]] ||
		die "The upstream vendor archive did not contain ${ECARGO_VENDOR}"
	[[ -f Cargo.lock ]] || die "Cargo.lock is required for an offline build"
	if use gui; then
		[[ -f gui/frontend/index.html ]] ||
			die "The upstream vendor archive omitted the production GUI frontend"
		[[ -f gui/frontend/app.js ]] ||
			die "The upstream vendor archive omitted the production GUI JavaScript"
		[[ -f gui/frontend/app.css ]] ||
			die "The upstream vendor archive omitted the production GUI stylesheet"
	fi
}

src_configure() {
	local myfeatures=(
		$(usev acoustid)
		$(usev alsa)
		$(usev ascii-visualizer)
		$(usev audio-quality)
		$(usev archive-rar)
		$(usev archive-zip)
		$(usev archive-org)
		$(usev archive-upload)
		$(usev apple-podcasts)
		$(usev bandcamp)
		$(usev bbc-radio)
		$(usev bilibili)
		$(usev bundled-sqlite)
		$(usev commons-upload)
		$(usev dearrow)
		$(usev discord)
		$(usev evernote)
		$(usev funkwhale)
		$(usev generic-ytdlp)
		$(usev google-drive)
		$(usev gpm)
		$(usev gpodder)
		$(usev images)
		$(usev invidious)
		$(usev jack)
		$(usev jamendo)
		$(usev keyring)
		$(usev lan-sharing)
		$(usev lastfm)
		$(usev librivox)
		$(usev litres)
		$(usev local)
		$(usev local-archives)
		$(usev nyan-cat)
		$(usev odysee)
		$(usev peertube)
		$(usev pipewire)
		$(usev podcast-index)
		$(usev pulseaudio)
		$(usev qr)
		$(usev radio)
		$(usev rss)
		$(usev rumble)
		$(usev rutube)
		$(usev soundcloud)
		$(usev soundstream)
		$(usev sponsorblock)
		$(usev sqlite sqlite-state)
		$(usev ssh)
		$(usev summary)
		$(usev youtube-captions)
		$(usev telegram)
		$(usev torrent)
		$(usev tracker-music)
		$(usev tui)
		$(usev vimeo)
		$(usev vk)
		$(usev waveform)
		$(usev web-browser)
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

src_compile() {
	cargo_src_compile

	# Upstream's desktop package carries a broad source set, with Archive.org
	# selected explicitly below so its USE flag also applies to the desktop.
	# Its external helpers are therefore declared by gui? above independently
	# of the granular USE flags that shape the terminal executable.
	if use gui; then
		local gui_features=tauri/custom-protocol
		if use archive-org; then
			gui_features+=,archive-org
		fi
		if use archive-upload; then
			gui_features+=,archive-upload
		fi
		if use ascii-visualizer; then
			gui_features+=,ascii-visualizer
		fi
		if use audio-quality; then
			gui_features+=,audio-quality
		fi
		if use commons-upload; then
			gui_features+=,commons-upload
		fi
		if use evernote; then
			gui_features+=,evernote
		fi
		if use lan-sharing; then
			gui_features+=,lan-sharing
		fi
		if use local-archives; then
			gui_features+=,local-archives
		fi
		if use nyan-cat; then
			gui_features+=,nyan-cat
		fi
		if use sponsorblock; then
			gui_features+=,sponsorblock
		fi
		if use summary; then
			gui_features+=,summary
		fi
		if use web-browser; then
			gui_features+=,web-browser
		fi
		if use youtube-captions; then
			gui_features+=,youtube-captions
		fi
		cargo_env "${CARGO}" build \
			$(usex debug "" --release) \
			--locked \
			--offline \
			--package youta-gui \
			--no-default-features \
			--features "${gui_features}" ||
			die "failed to build the desktop GUI"
	fi
}

src_test() {
	cargo_src_test

	if use gui; then
		local gui_feature_args=()
		local gui_test_features=
		if use ascii-visualizer; then
			gui_test_features=ascii-visualizer
		fi
		if use archive-org; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=archive-org
		fi
		if use audio-quality; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=audio-quality
		fi
		if use archive-upload; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=archive-upload
		fi
		if use commons-upload; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=commons-upload
		fi
		if use evernote; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=evernote
		fi
		if use lan-sharing; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=lan-sharing
		fi
		if use local-archives; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=local-archives
		fi
		if use nyan-cat; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=nyan-cat
		fi
		if use sponsorblock; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=sponsorblock
		fi
		if use summary; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=summary
		fi
		if use web-browser; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=web-browser
		fi
		if use youtube-captions; then
			[[ -n ${gui_test_features} ]] && gui_test_features+=,
			gui_test_features+=youtube-captions
		fi
		[[ -n ${gui_test_features} ]] &&
			gui_feature_args=( --features "${gui_test_features}" )
		cargo_env "${CARGO}" test \
			$(usex debug "" --release) \
			--locked \
			--offline \
			--package youta-gui \
			--all-targets \
			--no-default-features \
			"${gui_feature_args[@]}" || die "desktop GUI tests failed"
	fi
}

src_install() {
	cargo_src_install

	if use gui; then
		dobin "$(cargo_target_dir)/youta-gui"
	fi

	dodoc README.md config.example.toml
	dodoc docs/ARCHITECTURE.md docs/AUDIOPHILE.md docs/FEASIBILITY.md
}

pkg_postinst() {
	if use gui; then
		elog "The gui USE flag installed both youta and youta-gui."
	fi

	if use yt-dlp; then
		elog "yt-dlp support is opt-in at runtime and follows provider terms."
		elog "Keep net-misc/yt-dlp current because site extractors change."
	fi

	if use alsa && use mpv; then
		elog "List ALSA devices with: mpv --audio-device=help"
	fi

	if use gpm; then
		elog "GPM mouse input is used opportunistically on /dev/ttyN."
		elog "Physical mouse input requires the GPM daemon to be running."
		elog "On OpenRC: rc-service gpm start"
		elog "Enable it after reboot with: rc-update add gpm default"
		elog "F8 keeps Youta's keyboard pointer available without the daemon."
	fi
}
