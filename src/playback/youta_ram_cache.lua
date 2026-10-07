-- Preserve mpv's normal extractor and transport fallback. The application owns
-- all payloads; this bridge only redirects an eligible, resolved progressive
-- stream after the built-in yt-dlp hooks (priorities 10 and 20) have finished.
local utils = require 'mp.utils'
local pending = nil
local sequence = 0
local last_route = nil

-- Always release a deferred hook, including timeout, replacement and shutdown.
local function finish(route)
	local current = pending
	pending = nil
	if not current then return end
	if current.timer then current.timer:kill() end
	if route and route:match('^http://127%.0%.0%.1:%d+/')
		and not mp.get_property_bool('playback-abort', false) then
		last_route = route
		pcall(mp.set_property, 'stream-open-filename', route)
	end
	current.hook:cont()
end

-- Read the exact effective headers rather than asking yt-dlp a second time.
-- Cookie-file transports need separate domain-aware handling and stay on mpv.
local function headers()
	local result = mp.get_property_native('options/http-header-fields', {})
	if type(result) ~= 'table' or #result > 30 then return nil end
	local has_user_agent, has_referer, has_cookie = false, false, false
	local size = 0
	for _, field in ipairs(result) do
		if type(field) ~= 'string' then return nil end
		size = size + #field
		local name = field:match('^([^:]+):')
		name = name and name:lower()
		has_user_agent = has_user_agent or name == 'user-agent'
		has_referer = has_referer or name == 'referer'
		has_cookie = has_cookie or name == 'cookie'
	end
	if size > 16384 then return nil end
	if mp.get_property_bool('options/cookies', false)
		and mp.get_property('options/cookies-file', '') ~= ''
		and not has_cookie then return nil end
	local user_agent = mp.get_property('options/user-agent', '')
	if not has_user_agent and user_agent ~= '' then
		result[#result + 1] = 'User-Agent: ' .. user_agent
	end
	local referer = mp.get_property('options/referrer', '')
	if not has_referer and referer ~= '' then
		result[#result + 1] = 'Referer: ' .. referer
	end
	return result
end

-- Per-file script options bind each request to the application's load epoch.
-- A zero or absent generation disables interception, including every retry.
local function route(hook)
	local options = mp.get_property_native('options/script-opts', {})
	local generation = tonumber(options['youta_ram_cache-generation']) or 0
	if generation <= 0 then return end
	local source = mp.get_property('stream-open-filename', '')
	-- A failed proxy open must fall back, never recursively wrap its own URL.
	if source == last_route or #source > 8192 or not source:match('^https?://') then return end
	local fields = headers()
	if not fields then return end
	finish(nil)
	sequence = sequence + 1
	hook:defer()
	pending = { hook = hook, generation = generation, sequence = sequence }
	pending.timer = mp.add_timeout(1, function() finish(nil) end)
	mp.commandv('script-message', 'youta-ram-cache-register',
		tostring(generation), tostring(sequence), source, utils.format_json(fields))
end

mp.register_script_message('youta-ram-cache-route', function(generation, nonce, route_url)
	if pending and pending.generation == tonumber(generation)
		and pending.sequence == tonumber(nonce) then
		finish(route_url)
	end
end)

mp.add_hook('on_load', 50, route)
mp.add_hook('on_load_fail', 50, route)
mp.register_event('shutdown', function() finish(nil) end)
