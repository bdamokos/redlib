// @license http://www.gnu.org/licenses/agpl-3.0.html AGPL-3.0
(function () {
    var AudioContextClass = window.AudioContext || window.webkitAudioContext;
    // Keep speech/music previews near a comfortable level without aggressively
    // amplifying silence or allowing a quiet clip to receive excessive makeup gain.
    var TARGET_RMS = 0.16;
    var SILENCE_RMS = 0.008;
    var MIN_GAIN = 0.35;
    var MAX_GAIN = 2.5;
    var METER_INTERVAL_MS = 100;

    var audioContext = null;
    var records = new WeakMap();
    var activeVideo = null;
    var meterTimer = null;

    function supported() {
        return Boolean(AudioContextClass);
    }

    function ready() {
        return Boolean(audioContext && audioContext.state === "running");
    }

    function unlock() {
        if (!supported()) {
            return Promise.resolve(false);
        }

        if (!audioContext) {
            audioContext = new AudioContextClass();
        }

        if (audioContext.state === "running") {
            return Promise.resolve(true);
        }

        return audioContext.resume().then(function () {
            return audioContext.state === "running";
        }).catch(function () {
            return false;
        });
    }

    function sourceIsSameOrigin(video) {
        var source = video.currentSrc || video.getAttribute("src") || "";
        if (!source) {
            return false;
        }

        try {
            return new URL(source, window.location.href).origin === window.location.origin;
        } catch (error) {
            return false;
        }
    }

    function setParam(param, value, timeConstant) {
        if (!audioContext) {
            return;
        }

        param.cancelScheduledValues(audioContext.currentTime);
        param.setTargetAtTime(value, audioContext.currentTime, timeConstant);
    }

    function createRecord(video) {
        if (!ready() || !sourceIsSameOrigin(video)) {
            return null;
        }

        var existing = records.get(video);
        if (existing) {
            return existing;
        }

        try {
            var source = audioContext.createMediaElementSource(video);
            var analyser = audioContext.createAnalyser();
            var gate = audioContext.createGain();
            var adaptiveGain = audioContext.createGain();
            var limiter = audioContext.createDynamicsCompressor();

            analyser.fftSize = 1024;
            analyser.smoothingTimeConstant = 0.35;
            gate.gain.value = 0;
            adaptiveGain.gain.value = 1;
            limiter.threshold.value = -8;
            limiter.knee.value = 4;
            limiter.ratio.value = 12;
            limiter.attack.value = 0.003;
            limiter.release.value = 0.25;

            // The analyser measures the unmodified signal. The separate gated
            // branch is the only route to the speakers after createMediaElementSource.
            source.connect(analyser);
            source.connect(gate);
            gate.connect(adaptiveGain);
            adaptiveGain.connect(limiter);
            limiter.connect(audioContext.destination);

            var record = {
                analyser: analyser,
                gate: gate,
                adaptiveGain: adaptiveGain,
                samples: new Float32Array(analyser.fftSize),
                smoothedGain: 1
            };
            records.set(video, record);
            return record;
        } catch (error) {
            return null;
        }
    }

    function measuredRms(record) {
        record.analyser.getFloatTimeDomainData(record.samples);
        var sumSquares = 0;
        for (var index = 0; index < record.samples.length; index += 1) {
            var sample = record.samples[index];
            sumSquares += sample * sample;
        }
        return Math.sqrt(sumSquares / record.samples.length);
    }

    function meterActiveVideo() {
        meterTimer = null;
        if (!activeVideo || !ready()) {
            return;
        }

        var record = records.get(activeVideo);
        if (!record) {
            return;
        }

        var rms = measuredRms(record);
        if (Number.isFinite(rms) && rms >= SILENCE_RMS) {
            var desiredGain = Math.max(MIN_GAIN, Math.min(MAX_GAIN, TARGET_RMS / rms));
            var smoothing = desiredGain < record.smoothedGain ? 0.35 : 0.08;
            record.smoothedGain += (desiredGain - record.smoothedGain) * smoothing;
            setParam(record.adaptiveGain.gain, record.smoothedGain, desiredGain < record.smoothedGain ? 0.05 : 0.4);
        }

        meterTimer = window.setTimeout(meterActiveVideo, METER_INTERVAL_MS);
    }

    function stopMeter() {
        if (meterTimer) {
            window.clearTimeout(meterTimer);
            meterTimer = null;
        }
    }

    function silence(video) {
        var record = video ? records.get(video) : null;
        if (record) {
            setParam(record.gate.gain, 0, 0.015);
        }
        if (activeVideo === video) {
            activeVideo = null;
            stopMeter();
        }
    }

    function normalize(video) {
        if (!video || !ready()) {
            return false;
        }

        if (activeVideo && activeVideo !== video) {
            silence(activeVideo);
        }

        var record = createRecord(video);
        if (!record) {
            return false;
        }

        activeVideo = video;
        video.volume = 1;
        video.muted = false;
        setParam(record.gate.gain, 1, 0.015);
        if (!meterTimer) {
            meterTimer = window.setTimeout(meterActiveVideo, METER_INTERVAL_MS);
        }
        return true;
    }

    window.RedlibPreviewAudioNormalizer = {
        normalize: normalize,
        ready: ready,
        silence: silence,
        supported: supported,
        unlock: unlock
    };
})();
// @license-end
