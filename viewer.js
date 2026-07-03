/**
 * OpenTeslaCam
 * Multi-camera synchronized playback with vehicle data overlay
 */

(function() {
    'use strict';

    // Camera identifiers in display order
    const CAMERAS = ['front', 'left_repeater', 'right_repeater', 'back', 'left_pillar', 'right_pillar'];
    const CAMERA_LABELS = {
        front: 'Front',
        back: 'Back',
        left_repeater: 'Left Repeater',
        right_repeater: 'Right Repeater',
        left_pillar: 'Left Pillar',
        right_pillar: 'Right Pillar'
    };

    // Filename pattern: YYYY-MM-DD_HH-MM-SS-{camera}.mp4
    const FILENAME_REGEX = /^(\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2})-(front|back|left_repeater|right_repeater|left_pillar|right_pillar)\.mp4$/i;

    // Preset layouts for GridStack (12-column grid, 6 rows)
    const PRESETS = {
        // Review: left views on the left, right views on the right,
        // front + rear stacked in the centre column.
        review: [
            { id: 'left_repeater',  x: 0, y: 0, w: 4, h: 3 },
            { id: 'front',          x: 4, y: 0, w: 4, h: 3 },
            { id: 'right_repeater', x: 8, y: 0, w: 4, h: 3 },
            { id: 'left_pillar',    x: 0, y: 3, w: 4, h: 3 },
            { id: 'back',           x: 4, y: 3, w: 4, h: 3 },
            { id: 'right_pillar',   x: 8, y: 3, w: 4, h: 3 }
        ],
        // Driving: big front view with a rear + side-repeater rail down the right.
        driving: [
            { id: 'front',          x: 0, y: 0, w: 8, h: 6 },
            { id: 'back',           x: 8, y: 0, w: 4, h: 2 },
            { id: 'left_repeater',  x: 8, y: 2, w: 4, h: 2 },
            { id: 'right_repeater', x: 8, y: 4, w: 4, h: 2 }
        ]
    };

    // State
    let state = {
        events: [],
        currentEvent: null,
        currentSegmentIndex: 0,
        segments: [],
        segmentDurations: [],
        totalDuration: 0,
        playing: false,
        playbackRate: 1,
        seiData: [],
        seiType: null,
        seiFields: [],
        useMetric: true,
        objectUrls: {},
        map: null,
        carMarker: null,
        currentLayer: 'street',
        // GridStack state
        grid: null,
        activeCameras: [],
        currentPreset: 'review',
        // Trim & Export state
        exportPanelOpen: false,
        trimIn: 0,
        trimOut: null,   // null = end of content
        isExporting: false,
        exportCancelFn: null
    };

    // DOM elements
    const elements = {
        openFolderBtn: document.getElementById('openFolderBtn'),
        dropOverlay: document.getElementById('dropOverlay'),
        eventList: document.getElementById('eventList'),
        videoGrid: document.getElementById('videoGrid'),
        gridEmptyPlaceholder: document.getElementById('gridEmptyPlaceholder'),
        playBtn: document.getElementById('playBtn'),
        playIcon: document.getElementById('playIcon'),
        pauseIcon: document.getElementById('pauseIcon'),
        scrubber: document.getElementById('scrubber'),
        timeDisplay: document.getElementById('timeDisplay'),
        seiContent: document.getElementById('seiContent'),
        seiHud: document.getElementById('seiHud'),
        presetBtns: document.querySelectorAll('.preset-btn'),
        unitBtns: document.querySelectorAll('.unit-toggle .btn'),
        addCameraBtn: document.getElementById('addCameraBtn'),
        cameraDropdown: document.getElementById('cameraDropdown'),
        addCameraPromptBtn: document.getElementById('addCameraPromptBtn')
    };

    // Video elements - dynamically managed
    const videos = {};

    // =========================================================================
    // INITIALIZATION
    // =========================================================================

    async function init() {
        // Initialize protobuf for SEI parsing. Keep the promise so that segment
        // parsing can await it — otherwise a clip loaded before init finishes
        // caches an empty SEI result that is never retried (vehicle data stays blank).
        state.protobufReady = (async () => {
            try {
                const { SeiMetadata, enumFields } = await DashcamHelpers.initProtobuf('dashcam-master/dashcam.proto');
                state.seiType = SeiMetadata;
                state.seiFields = DashcamHelpers.deriveFieldInfo(SeiMetadata, enumFields, { useLabels: true });
                console.log('Protobuf initialized successfully');
            } catch (err) {
                console.error('Failed to initialize protobuf:', err);
            }
        })();
        await state.protobufReady;

        setupEventListeners();
        
        // Initialize GridStack for the camera grid
        const gridInitialized = initGridStack();
        
        initMap();
        
        // Load saved layout or default to review preset (only if grid was initialized)
        if (gridInitialized) {
            loadLayoutFromStorage() || loadPreset('review');
        }
    }

    function setupEventListeners() {
        // Open folder button
        elements.openFolderBtn.addEventListener('click', openFolderPicker);

        // Events dock: hover reveals the panel (with a close delay so moving the
        // cursor from the button onto the panel doesn't dismiss it); click pins it.
        const eventsToggle = document.getElementById('eventsToggle');
        const sidebarEl = document.getElementById('sidebar');
        let sidebarHideTimer = null;
        const showSidebar = () => {
            clearTimeout(sidebarHideTimer);
            sidebarEl?.classList.add('hover');
        };
        const hideSidebarSoon = () => {
            clearTimeout(sidebarHideTimer);
            sidebarHideTimer = setTimeout(() => sidebarEl?.classList.remove('hover'), 250);
        };
        eventsToggle?.addEventListener('mouseenter', showSidebar);
        eventsToggle?.addEventListener('mouseleave', hideSidebarSoon);
        sidebarEl?.addEventListener('mouseenter', showSidebar);
        sidebarEl?.addEventListener('mouseleave', hideSidebarSoon);
        eventsToggle?.addEventListener('click', () => {
            const open = sidebarEl.classList.toggle('open');
            eventsToggle.classList.toggle('active', open);
        });

        // Event category filters (All / Saved / Sentry)
        document.querySelectorAll('.event-filter-btn').forEach(btn => {
            btn.addEventListener('click', () => {
                state.eventFilter = btn.dataset.filter;
                document.querySelectorAll('.event-filter-btn').forEach(b => b.classList.toggle('active', b === btn));
                renderEventList();
            });
        });

        // Drag and drop
        document.body.addEventListener('dragover', handleDragOver);
        document.body.addEventListener('dragleave', handleDragLeave);
        document.body.addEventListener('drop', handleDrop);

        // Playback controls
        elements.playBtn.addEventListener('click', togglePlayPause);
        elements.scrubber.addEventListener('input', handleScrub);

        // Preset buttons
        elements.presetBtns.forEach(btn => {
            btn.addEventListener('click', () => loadPreset(btn.dataset.preset));
        });

        // Add camera dropdown
        elements.addCameraBtn?.addEventListener('click', (e) => {
            e.stopPropagation();
            toggleCameraDropdown();
        });
        elements.addCameraPromptBtn?.addEventListener('click', (e) => {
            e.stopPropagation();
            toggleCameraDropdown();
        });
        
        document.querySelectorAll('.dropdown-item[data-camera]').forEach(item => {
            item.addEventListener('click', (e) => {
                e.stopPropagation();
                const cameraId = item.dataset.camera;
                // Check if camera is already in grid (disabled state)
                if (item.classList.contains('disabled')) {
                    return;
                }
                if (!state.activeCameras.includes(cameraId)) {
                    addCamera(cameraId);
                }
                closeCameraDropdown();
            });
        });

        // Close dropdown when clicking outside
        document.addEventListener('click', (e) => {
            if (!e.target.closest('.add-camera-dropdown') && !e.target.closest('#addCameraPromptBtn')) {
                closeCameraDropdown();
            }
        });

        // Unit toggle
        elements.unitBtns.forEach(btn => {
            btn.addEventListener('click', () => setUnit(btn.dataset.unit));
        });

        // Playback speed
        document.querySelectorAll('.speed-btn').forEach(btn => {
            btn.addEventListener('click', () => setPlaybackRate(parseFloat(btn.dataset.rate)));
        });

        // Help / shortcuts modal
        document.getElementById('helpBtn')?.addEventListener('click', toggleShortcutsModal);
        document.getElementById('closeShortcutsBtn')?.addEventListener('click', toggleShortcutsModal);
        document.getElementById('shortcutsBackdrop')?.addEventListener('click', toggleShortcutsModal);

        // Export / trim panel
        document.getElementById('exportToggleBtn')?.addEventListener('click', toggleExportPanel);
        document.getElementById('setInBtn')?.addEventListener('click', setTrimIn);
        document.getElementById('setOutBtn')?.addEventListener('click', setTrimOut);
        document.getElementById('exportBtn')?.addEventListener('click', startExport);
        document.getElementById('cancelExportBtn')?.addEventListener('click', cancelExport);

        // Keyboard shortcuts
        document.addEventListener('keydown', handleKeydown);

        // Note: Video event listeners are set up dynamically in setupMasterVideoEvents()
        // when cameras are added to the grid
    }

    // =========================================================================
    // GRIDSTACK / CAMERA GRID
    // =========================================================================

    function initGridStack() {
        if (typeof GridStack === 'undefined') {
            console.error('GridStack not loaded');
            return false;
        }

        if (!elements.videoGrid) {
            console.error('Video grid element not found');
            return false;
        }

        try {
            state.grid = GridStack.init({
                column: 12,
                row: 6,
                cellHeight: 'auto',
                margin: 2,
                float: true,
                disableOneColumnMode: true,
                animate: true,
                minRow: 1,
                acceptWidgets: true,
                removable: false // We handle removal ourselves
            }, elements.videoGrid);

            if (!state.grid) {
                console.error('GridStack.init returned null/undefined');
                return false;
            }

            // Save layout when items change
            state.grid.on('change', () => {
                saveLayoutToStorage();
                state.currentPreset = null;
                updatePresetButtons();
            });

            // Keep the camera grid filling the available vertical space: as the
            // segment/export bars appear or disappear, the flex container resizes
            // and we recompute the cell height so the feeds shrink/grow to fit.
            if (typeof ResizeObserver !== 'undefined') {
                state.gridResizeObserver = new ResizeObserver(() => resizeGridToFit());
                state.gridResizeObserver.observe(elements.videoGrid);
            }
            resizeGridToFit();

            console.log('GridStack initialized');
            return true;
        } catch (err) {
            console.error('GridStack initialization failed:', err);
            return false;
        }
    }

    // Size GridStack cells so the 6-row grid exactly fills the container's current
    // height. Called whenever the available space changes (bars toggling, resize).
    function resizeGridToFit() {
        if (!state.grid || !elements.videoGrid) return;
        const h = elements.videoGrid.clientHeight;
        if (h <= 0) return;
        const target = h / 6; // the grid is always 6 rows tall
        const current = state.grid.getCellHeight(true) || 0;
        if (Math.abs(current - target) > 0.5) {
            state.grid.cellHeight(target);
        }
    }

    function createCameraWidget(cameraId) {
        const label = CAMERA_LABELS[cameraId] || cameraId;

        return `
            <div class="camera-widget" data-camera="${cameraId}" ondblclick="window.fullscreenCamera('${cameraId}')">
                <video id="video-${cameraId}" muted playsinline></video>
                <span class="video-label">${label}</span>
                <div class="widget-actions">
                    <button class="action-btn" onclick="event.stopPropagation(); window.snapshotCamera('${cameraId}')" title="Save snapshot">
                        <svg viewBox="0 0 24 24"><path fill="currentColor" d="M12 15.2A3.2 3.2 0 0 1 8.8 12 3.2 3.2 0 0 1 12 8.8 3.2 3.2 0 0 1 15.2 12 3.2 3.2 0 0 1 12 15.2M20 4h-3.17L15 2H9L7.17 4H4a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2m-8 13a5 5 0 0 1-5-5 5 5 0 0 1 5-5 5 5 0 0 1 5 5 5 5 0 0 1-5 5z"/></svg>
                    </button>
                    <button class="action-btn" onclick="event.stopPropagation(); window.fullscreenCamera('${cameraId}')" title="Fullscreen (or double-click)">
                        <svg viewBox="0 0 24 24"><path fill="currentColor" d="M7 14H5v5h5v-2H7v-3zm-2-4h2V7h3V5H5v5zm12 7h-3v2h5v-5h-2v3zM14 5v2h3v3h2V5h-5z"/></svg>
                    </button>
                    <button class="action-btn" onclick="event.stopPropagation(); window.removeCamera('${cameraId}')" title="Remove camera">
                        <svg viewBox="0 0 24 24"><path fill="currentColor" d="M19 6.41L17.59 5 12 10.59 6.41 5 5 6.41 10.59 12 5 17.59 6.41 19 12 13.41 17.59 19 19 17.59 13.41 12z"/></svg>
                    </button>
                </div>
            </div>
        `;
    }

    function addCamera(cameraId, options = {}) {
        if (!state.grid) {
            console.error('GridStack not initialized, cannot add camera');
            return;
        }

        if (state.activeCameras.includes(cameraId)) {
            console.warn(`Camera ${cameraId} already in grid`);
            return;
        }

        // Default position/size if not specified
        const defaults = {
            w: 4,
            h: 3,
            x: undefined, // Let GridStack find position
            y: undefined
        };
        const opts = { ...defaults, ...options };

        const widgetHtml = createCameraWidget(cameraId);
        
        state.grid.addWidget({
            id: cameraId,
            x: opts.x,
            y: opts.y,
            w: opts.w,
            h: opts.h,
            minW: 2,
            minH: 1,
            content: widgetHtml
        });

        state.activeCameras.push(cameraId);
        
        // Get the video element reference
        videos[cameraId] = document.getElementById(`video-${cameraId}`);
        
        // If we have an active segment, load this camera's video
        if (state.segments.length > 0 && state.currentSegmentIndex >= 0) {
            loadCameraVideo(cameraId);
        }

        // Rebind master video events whenever camera lineup changes
        setupMasterVideoEvents();

        updateGridPlaceholder();
        updateCameraDropdown();
        saveLayoutToStorage();
    }

    // Expose camera actions globally for inline onclick handlers
    window.removeCamera = function(cameraId) {
        removeCamera(cameraId);
    };

    window.fullscreenCamera = function(cameraId) {
        const widget = document.querySelector(`.camera-widget[data-camera="${cameraId}"]`);
        if (!widget) return;
        if (document.fullscreenElement) {
            document.exitFullscreen();
        } else {
            widget.requestFullscreen().catch(() => {});
        }
    };

    window.snapshotCamera = function(cameraId) {
        const video = videos[cameraId];
        if (!video || !video.videoWidth) {
            console.warn(`No video frame available for ${cameraId}`);
            return;
        }
        const canvas = document.createElement('canvas');
        canvas.width = video.videoWidth;
        canvas.height = video.videoHeight;
        canvas.getContext('2d').drawImage(video, 0, 0);
        const link = document.createElement('a');
        const timeStr = formatTime(video.currentTime).replace(/:/g, '-');
        link.download = `${cameraId}-${timeStr}.png`;
        link.href = canvas.toDataURL('image/png');
        link.click();
    };

    function removeCamera(cameraId) {
        if (!state.grid) return;
        
        const index = state.activeCameras.indexOf(cameraId);
        if (index === -1) return;

        // Find and remove the widget
        const widgets = state.grid.getGridItems();
        const widget = widgets.find(w => w.gridstackNode?.id === cameraId);
        if (widget) {
            state.grid.removeWidget(widget);
        }

        // Clean up video
        if (videos[cameraId]) {
            videos[cameraId].pause();
            videos[cameraId].src = '';
            delete videos[cameraId];
        }

        // Revoke object URL
        if (state.objectUrls[cameraId]) {
            URL.revokeObjectURL(state.objectUrls[cameraId]);
            delete state.objectUrls[cameraId];
        }

        state.activeCameras.splice(index, 1);

        // Rebind master events to whatever camera is now the master
        setupMasterVideoEvents();

        updateGridPlaceholder();
        updateCameraDropdown();
        saveLayoutToStorage();
    }

    function loadCameraVideo(cameraId) {
        const segment = state.segments[state.currentSegmentIndex];
        if (!segment) return;

        const file = segment.files[cameraId];
        const video = videos[cameraId];
        const widget = document.querySelector(`.camera-widget[data-camera="${cameraId}"]`);

        if (file && video) {
            if (state.objectUrls[cameraId]) {
                URL.revokeObjectURL(state.objectUrls[cameraId]);
            }
            state.objectUrls[cameraId] = URL.createObjectURL(file);
            video.src = state.objectUrls[cameraId];
            video.playbackRate = state.playbackRate;
            video.load();
            widget?.classList.remove('no-video');
        } else if (video) {
            video.removeAttribute('src');
            video.load();
            widget?.classList.add('no-video');
        }
    }

    function setupMasterVideoEvents() {
        // Unbind from all cameras first to avoid duplicate listeners
        for (const cam of CAMERAS) {
            if (videos[cam]) {
                videos[cam].removeEventListener('timeupdate', handleTimeUpdate);
                videos[cam].removeEventListener('ended', handleSegmentEnd);
                videos[cam].removeEventListener('loadedmetadata', handleVideoLoaded);
            }
        }

        // Bind to front camera if active, otherwise first available active camera
        const masterCam = state.activeCameras.includes('front')
            ? 'front'
            : state.activeCameras[0];

        const masterVideo = masterCam ? videos[masterCam] : null;
        if (!masterVideo) return;

        masterVideo.addEventListener('timeupdate', handleTimeUpdate);
        masterVideo.addEventListener('ended', handleSegmentEnd);
        masterVideo.addEventListener('loadedmetadata', handleVideoLoaded);
    }

    function loadPreset(presetName) {
        if (!state.grid) {
            console.error('GridStack not initialized, cannot load preset');
            return;
        }

        const preset = PRESETS[presetName];
        if (!preset) {
            console.error(`Preset "${presetName}" not found`);
            return;
        }

        // Clear current grid
        state.grid.removeAll();
        state.activeCameras = [];
        
        // Clear video references
        Object.keys(videos).forEach(key => delete videos[key]);

        // Add cameras from preset
        preset.forEach(cam => {
            addCamera(cam.id, { x: cam.x, y: cam.y, w: cam.w, h: cam.h });
        });

        state.currentPreset = presetName;
        updatePresetButtons();
        // Persist with the final preset name — the per-addCamera saves above ran
        // while currentPreset still held the previous value, so without this the
        // restored layout and the highlighted preset button can disagree.
        saveLayoutToStorage();

        // Setup master video events after adding cameras
        setupMasterVideoEvents();

        console.log(`Loaded preset: ${presetName}`);
    }

    function updatePresetButtons() {
        elements.presetBtns.forEach(btn => {
            btn.classList.toggle('active', btn.dataset.preset === state.currentPreset);
        });
    }

    function updateGridPlaceholder() {
        const isEmpty = state.activeCameras.length === 0;
        elements.gridEmptyPlaceholder?.classList.toggle('hidden', !isEmpty);
    }

    function toggleCameraDropdown() {
        elements.cameraDropdown?.classList.toggle('open');
        updateCameraDropdown();
    }

    function closeCameraDropdown() {
        elements.cameraDropdown?.classList.remove('open');
    }

    function updateCameraDropdown() {
        document.querySelectorAll('.dropdown-item[data-camera]').forEach(item => {
            const cameraId = item.dataset.camera;
            const isActive = state.activeCameras.includes(cameraId);
            item.classList.toggle('disabled', isActive);
        });
    }

    function saveLayoutToStorage() {
        const layout = state.grid.save(false); // Don't save content, just positions
        const layoutData = {
            preset: state.currentPreset,
            items: layout.map(item => ({
                id: item.id,
                x: item.x,
                y: item.y,
                w: item.w,
                h: item.h
            }))
        };
        localStorage.setItem('openTeslaCamLayout', JSON.stringify(layoutData));
    }

    function loadLayoutFromStorage() {
        if (!state.grid) return false;
        
        try {
            const saved = localStorage.getItem('openTeslaCamLayout');
            if (!saved) return false;

            const layoutData = JSON.parse(saved);
            if (!layoutData.items || layoutData.items.length === 0) return false;

            // Clear current grid
            state.grid.removeAll();
            state.activeCameras = [];
            Object.keys(videos).forEach(key => delete videos[key]);

            // Add cameras from saved layout
            layoutData.items.forEach(item => {
                if (CAMERAS.includes(item.id)) {
                    addCamera(item.id, { x: item.x, y: item.y, w: item.w, h: item.h });
                }
            });

            state.currentPreset = layoutData.preset;
            updatePresetButtons();
            setupMasterVideoEvents();

            console.log('Loaded layout from storage');
            return true;
        } catch (err) {
            console.error('Failed to load layout from storage:', err);
            return false;
        }
    }

    // =========================================================================
    // FOLDER / EVENT DISCOVERY
    // =========================================================================

    async function openFolderPicker() {
        if (!window.showDirectoryPicker) {
            alert('Your browser does not support the File System Access API. Please use drag and drop instead.');
            return;
        }

        try {
            const dirHandle = await window.showDirectoryPicker();
            await processDirectory(dirHandle);
        } catch (err) {
            if (err.name !== 'AbortError') {
                console.error('Error opening folder:', err);
            }
        }
    }

    function handleDragOver(e) {
        e.preventDefault();
        e.stopPropagation();
        elements.dropOverlay.classList.add('dragover');
        elements.dropOverlay.classList.remove('hidden');
    }

    function handleDragLeave(e) {
        e.preventDefault();
        e.stopPropagation();
        elements.dropOverlay.classList.remove('dragover');
        if (state.events.length > 0) {
            elements.dropOverlay.classList.add('hidden');
        }
    }

    async function handleDrop(e) {
        e.preventDefault();
        e.stopPropagation();
        elements.dropOverlay.classList.remove('dragover');

        const items = e.dataTransfer.items;
        if (!items || items.length === 0) return;

        // Try to get directory handle (modern API)
        const item = items[0];
        if (item.kind === 'file') {
            const entry = item.webkitGetAsEntry?.();
            if (entry && entry.isDirectory) {
                await processDirectoryEntry(entry);
            }
        }
    }

    async function processDirectory(dirHandle) {
        const events = [];
        const structure = await scanDirectoryHandle(dirHandle);
        
        // Check if this is TeslaCam root or an event folder
        const isTeslaCamRoot = structure.children.some(c => 
            ['SavedClips', 'SentryClips', 'RecentClips'].includes(c.name)
        );

        if (isTeslaCamRoot) {
            // Scan each subfolder type
            for (const child of structure.children) {
                if (child.isDirectory && ['SavedClips', 'SentryClips', 'RecentClips'].includes(child.name)) {
                    const type = child.name.replace('Clips', '').toLowerCase();
                    const subHandle = await dirHandle.getDirectoryHandle(child.name);
                    
                    if (child.name === 'RecentClips') {
                        // RecentClips is flat - treat as single event
                        const recentFiles = await getFilesFromHandle(subHandle);
                        if (recentFiles.length > 0) {
                            events.push({
                                id: 'recent',
                                name: 'Recent Clips',
                                type: 'recent',
                                handle: subHandle,
                                files: recentFiles,
                                thumb: null
                            });
                        }
                    } else {
                        // SavedClips/SentryClips have event subfolders
                        for await (const [name, handle] of subHandle) {
                            if (handle.kind === 'directory') {
                                const eventFiles = await getFilesFromHandle(handle);
                                const eventJson = await tryGetFile(handle, 'event.json');
                                const thumbFile = await tryGetFile(handle, 'thumb.png');
                                
                                events.push({
                                    id: name,
                                    name: name,
                                    type: type,
                                    handle: handle,
                                    files: eventFiles,
                                    eventData: eventJson ? JSON.parse(await eventJson.text()) : null,
                                    thumb: thumbFile
                                });
                            }
                        }
                    }
                }
            }
        } else {
            // Treat as single event folder
            const files = await getFilesFromHandle(dirHandle);
            const eventJson = await tryGetFile(dirHandle, 'event.json');
            const thumbFile = await tryGetFile(dirHandle, 'thumb.png');
            
            events.push({
                id: dirHandle.name,
                name: dirHandle.name,
                type: 'saved',
                handle: dirHandle,
                files: files,
                eventData: eventJson ? JSON.parse(await eventJson.text()) : null,
                thumb: thumbFile
            });
        }

        // Sort events by name (which includes timestamp)
        events.sort((a, b) => b.name.localeCompare(a.name));
        
        state.events = events;
        renderEventList();
        
        if (events.length > 0) {
            elements.dropOverlay.classList.add('hidden');
            selectEvent(events[0]);
        }
    }

    async function processDirectoryEntry(entry) {
        const events = [];
        const children = await readDirectoryEntry(entry);
        
        const isTeslaCamRoot = children.some(c => 
            c.isDirectory && ['SavedClips', 'SentryClips', 'RecentClips'].includes(c.name)
        );

        if (isTeslaCamRoot) {
            for (const child of children) {
                if (child.isDirectory && ['SavedClips', 'SentryClips', 'RecentClips'].includes(child.name)) {
                    const type = child.name.replace('Clips', '').toLowerCase();
                    
                    if (child.name === 'RecentClips') {
                        const files = await getFilesFromEntry(child);
                        if (files.length > 0) {
                            events.push({
                                id: 'recent',
                                name: 'Recent Clips',
                                type: 'recent',
                                entry: child,
                                files: files,
                                thumb: null
                            });
                        }
                    } else {
                        const eventFolders = await readDirectoryEntry(child);
                        for (const eventFolder of eventFolders) {
                            if (eventFolder.isDirectory) {
                                const eventFiles = await getFilesFromEntry(eventFolder);
                                const eventJson = await tryGetFileFromEntry(eventFolder, 'event.json');
                                const thumbFile = await tryGetFileFromEntry(eventFolder, 'thumb.png');
                                
                                events.push({
                                    id: eventFolder.name,
                                    name: eventFolder.name,
                                    type: type,
                                    entry: eventFolder,
                                    files: eventFiles,
                                    eventData: eventJson ? JSON.parse(await eventJson.text()) : null,
                                    thumb: thumbFile
                                });
                            }
                        }
                    }
                }
            }
        } else {
            const files = await getFilesFromEntry(entry);
            const eventJson = await tryGetFileFromEntry(entry, 'event.json');
            const thumbFile = await tryGetFileFromEntry(entry, 'thumb.png');
            
            events.push({
                id: entry.name,
                name: entry.name,
                type: 'saved',
                entry: entry,
                files: files,
                eventData: eventJson ? JSON.parse(await eventJson.text()) : null,
                thumb: thumbFile
            });
        }

        events.sort((a, b) => b.name.localeCompare(a.name));
        
        state.events = events;
        renderEventList();
        
        if (events.length > 0) {
            elements.dropOverlay.classList.add('hidden');
            selectEvent(events[0]);
        }
    }

    // =========================================================================
    // FILE SYSTEM HELPERS
    // =========================================================================

    async function scanDirectoryHandle(dirHandle) {
        const children = [];
        for await (const [name, handle] of dirHandle) {
            children.push({
                name,
                isDirectory: handle.kind === 'directory'
            });
        }
        return { name: dirHandle.name, children };
    }

    async function getFilesFromHandle(dirHandle) {
        const files = [];
        for await (const [name, handle] of dirHandle) {
            if (handle.kind === 'file' && name.toLowerCase().endsWith('.mp4')) {
                files.push(await handle.getFile());
            }
        }
        return files;
    }

    async function tryGetFile(dirHandle, filename) {
        try {
            const fileHandle = await dirHandle.getFileHandle(filename);
            return await fileHandle.getFile();
        } catch {
            return null;
        }
    }

    async function readDirectoryEntry(entry) {
        return new Promise((resolve, reject) => {
            const reader = entry.createReader();
            const entries = [];
            
            function readEntries() {
                reader.readEntries(results => {
                    if (results.length === 0) {
                        resolve(entries);
                    } else {
                        entries.push(...results);
                        readEntries();
                    }
                }, reject);
            }
            readEntries();
        });
    }

    async function getFilesFromEntry(entry) {
        const entries = await readDirectoryEntry(entry);
        const files = [];
        
        for (const e of entries) {
            if (e.isFile && e.name.toLowerCase().endsWith('.mp4')) {
                const file = await new Promise((res, rej) => e.file(res, rej));
                files.push(file);
            }
        }
        return files;
    }

    async function tryGetFileFromEntry(dirEntry, filename) {
        const entries = await readDirectoryEntry(dirEntry);
        for (const e of entries) {
            if (e.isFile && e.name === filename) {
                return new Promise((res, rej) => e.file(res, rej));
            }
        }
        return null;
    }

    // =========================================================================
    // EVENT LIST UI
    // =========================================================================

    function renderEventList() {
        if (state.events.length === 0) {
            elements.eventList.innerHTML = '<p class="placeholder">No events found</p>';
            return;
        }

        // Category filter: all / saved / sentry
        const filter = state.eventFilter || 'all';
        const events = filter === 'all'
            ? state.events
            : state.events.filter(e => e.type === filter);

        if (events.length === 0) {
            elements.eventList.innerHTML = `<p class="placeholder">No ${filter} clips</p>`;
            return;
        }

        elements.eventList.innerHTML = events.map(event => `
            <div class="event-item" data-id="${event.id}">
                <img class="thumb" src="${event.thumb ? URL.createObjectURL(event.thumb) : 'data:image/svg+xml,<svg xmlns=%22http://www.w3.org/2000/svg%22/>'}" alt="">
                <div class="info">
                    <span class="name">${formatEventName(event)}</span>
                    <span class="meta">
                        <span class="event-badge ${event.type}">${event.type}</span>
                        ${event.eventData?.city || ''} ${event.eventData?.street || ''}
                    </span>
                </div>
            </div>
        `).join('');

        // Add click handlers
        elements.eventList.querySelectorAll('.event-item').forEach(el => {
            el.addEventListener('click', () => {
                const event = state.events.find(e => e.id === el.dataset.id);
                if (event) selectEvent(event);
                collapseSidebar();
            });
        });
    }

    // Unpin/hide the auto-hiding events sidebar (e.g. after picking a clip)
    function collapseSidebar() {
        document.getElementById('sidebar')?.classList.remove('open');
        document.getElementById('eventsToggle')?.classList.remove('active');
    }

    function formatEventName(event) {
        if (event.name === 'Recent Clips') return event.name;
        // Parse timestamp from folder name: 2026-01-27_16-31-59
        const match = event.name.match(/^(\d{4})-(\d{2})-(\d{2})_(\d{2})-(\d{2})-(\d{2})$/);
        if (match) {
            const [, y, m, d, h, min] = match;
            return `${m}/${d}/${y} ${h}:${min}`;
        }
        return event.name;
    }

    async function selectEvent(event) {
        // Stop current playback
        pause();
        revokeObjectUrls();

        // Update UI
        elements.eventList.querySelectorAll('.event-item').forEach(el => {
            el.classList.toggle('active', el.dataset.id === event.id);
        });

        state.currentEvent = event;
        
        // Build segments from files
        await buildSegments(event.files);
        
        // Load first segment
        if (state.segments.length > 0) {
            await loadSegment(0);
            elements.playBtn.disabled = false;
            elements.scrubber.disabled = false;
        }
    }

    // =========================================================================
    // SEGMENT MANAGEMENT
    // =========================================================================

    async function buildSegments(files) {
        const segmentMap = new Map();
        let matchedFiles = 0;
        let unmatchedFiles = [];
        
        // Group files by timestamp
        for (const file of files) {
            const match = file.name.match(FILENAME_REGEX);
            if (match) {
                const [, timestamp, camera] = match;
                if (!segmentMap.has(timestamp)) {
                    segmentMap.set(timestamp, {
                        timestamp,
                        files: {},
                        estimatedDuration: 60000 // Default 60s
                    });
                }
                segmentMap.get(timestamp).files[camera] = file;
                matchedFiles++;
            } else {
                unmatchedFiles.push(file.name);
            }
        }

        // Log parsing results
        console.log(`Parsed ${matchedFiles} MP4 files into ${segmentMap.size} segments`);
        if (unmatchedFiles.length > 0) {
            console.warn(`${unmatchedFiles.length} files did not match pattern:`, unmatchedFiles.slice(0, 5));
        }

        // Sort segments by timestamp
        state.segments = Array.from(segmentMap.values())
            .sort((a, b) => a.timestamp.localeCompare(b.timestamp));

        // Log segment details
        console.group('Segments detected:');
        for (let i = 0; i < state.segments.length; i++) {
            const segment = state.segments[i];
            const availableCameras = CAMERAS.filter(cam => segment.files[cam]);
            console.log(`  Segment ${i + 1}: ${segment.timestamp} (${availableCameras.length}/6 cameras)`);
        }
        console.groupEnd();

        // Estimate durations from timestamp differences
        // Tesla segments are typically ~60s, but we can be more precise using timestamps
        for (let i = 0; i < state.segments.length; i++) {
            if (i < state.segments.length - 1) {
                // Calculate duration from difference to next segment timestamp
                const currentTime = parseTimestamp(state.segments[i].timestamp);
                const nextTime = parseTimestamp(state.segments[i + 1].timestamp);
                if (currentTime && nextTime) {
                    const diffMs = nextTime - currentTime;
                    // Sanity check: Tesla segments are 30-120 seconds typically
                    if (diffMs > 0 && diffMs < 120000) {
                        state.segments[i].estimatedDuration = diffMs;
                    }
                }
            }
            // Last segment: keep default 60s, will be updated when loaded
        }

        // Initialize duration arrays
        state.segmentDurations = state.segments.map(s => s.estimatedDuration);
        state.totalDuration = state.segmentDurations.reduce((sum, d) => sum + d, 0);
        state.seiData = new Array(state.segments.length).fill(null);

        console.log(`Total estimated duration: ${formatTime(state.totalDuration / 1000)} (${state.segments.length} segments)`);
        
        // Update UI with segment info
        updateSegmentInfo();
        renderSegmentTimeline();
    }

    // Parse timestamp string "YYYY-MM-DD_HH-MM-SS" to Date
    function parseTimestamp(ts) {
        const match = ts.match(/^(\d{4})-(\d{2})-(\d{2})_(\d{2})-(\d{2})-(\d{2})$/);
        if (!match) return null;
        const [, year, month, day, hour, min, sec] = match.map(Number);
        return new Date(year, month - 1, day, hour, min, sec).getTime();
    }

    async function loadSegment(index) {
        if (index < 0 || index >= state.segments.length) return;
        
        const segment = state.segments[index];
        state.currentSegmentIndex = index;
        
        console.log(`Loading segment ${index + 1}/${state.segments.length}: ${segment.timestamp}`);
        
        // Create object URLs for each ACTIVE camera only
        for (const cam of state.activeCameras) {
            loadCameraVideo(cam);
        }

        // Parse SEI from front camera (if not already parsed)
        if (!state.seiData[index]) {
            await parseSeiForSegment(index, segment);
        }

        // Update UI
        updateScrubberRange();
        updateSegmentInfo();
        renderSegmentTimeline();

        // Show SEI data for the start of this segment immediately (don't wait for playback)
        const segmentStartMs = state.segmentDurations.slice(0, index).reduce((s, d) => s + d, 0);
        updateSeiDisplay(segmentStartMs);

        // Preload SEI for next segment in background
        if (index + 1 < state.segments.length && !state.seiData[index + 1]) {
            parseSeiForSegment(index + 1, state.segments[index + 1]).catch(() => {});
        }
    }

    function updateSegmentInfo() {
        const segmentInfo = document.getElementById('segmentInfo');
        if (segmentInfo && state.segments.length > 0) {
            const segment = state.segments[state.currentSegmentIndex];
            segmentInfo.textContent = `Segment ${state.currentSegmentIndex + 1}/${state.segments.length}`;
            segmentInfo.title = segment ? segment.timestamp : '';
        }
        
        // Update timeline highlighting
        const timelineItems = document.querySelectorAll('.segment-marker');
        timelineItems.forEach((item, index) => {
            item.classList.toggle('active', index === state.currentSegmentIndex);
        });
    }

    function renderSegmentTimeline() {
        const timeline = document.getElementById('segmentTimeline');
        const container = document.querySelector('.segment-timeline-container');
        if (!timeline || state.segments.length === 0) {
            container?.classList.add('hidden');
            return;
        }

        // The segment strip only adds value for multi-segment clips; for a single
        // segment the scrubber already covers it, so hide the strip.
        container?.classList.toggle('hidden', state.segments.length <= 1);

        // Create segment markers - played segments fill in, the current one is active
        timeline.innerHTML = state.segments.map((segment, index) => {
            const widthPercent = (state.segmentDurations[index] / state.totalDuration) * 100;
            const time = segment.timestamp.split('_')[1].replace(/-/g, ':'); // Format: HH:MM:SS
            let cls = 'segment-marker';
            if (index === state.currentSegmentIndex) cls += ' active';
            else if (index < state.currentSegmentIndex) cls += ' played';
            return `<div class="${cls}"
                        data-index="${index}"
                        style="width: ${widthPercent}%"
                        title="Segment ${index + 1}: ${time}"></div>`;
        }).join('');

        // Add click handlers to jump to segments
        timeline.querySelectorAll('.segment-marker').forEach(marker => {
            marker.addEventListener('click', () => {
                const index = parseInt(marker.dataset.index);
                jumpToSegment(index);
            });
        });
    }

    async function jumpToSegment(index) {
        if (index < 0 || index >= state.segments.length) return;
        
        // Calculate global time at start of this segment
        let globalTime = 0;
        for (let i = 0; i < index; i++) {
            globalTime += state.segmentDurations[i];
        }
        
        await seekToGlobalTime(globalTime);
    }

    async function parseSeiForSegment(index, segment) {
        // Ensure protobuf init has completed before deciding there's no SEI type
        // (guards against the load-before-init race).
        if (state.protobufReady) {
            try { await state.protobufReady; } catch (e) { /* handled in init */ }
        }
        if (!state.seiType) {
            state.seiData[index] = [];
            return;
        }

        // Prefer front camera for SEI parsing; fall back to any available camera
        const seiCam = segment.files.front
            ? 'front'
            : CAMERAS.find(cam => segment.files[cam]);

        if (!seiCam) {
            state.seiData[index] = [];
            return;
        }

        try {
            const file = segment.files[seiCam];
            const buffer = await file.arrayBuffer();
            const mp4 = new DashcamMP4(buffer);
            const frames = mp4.parseFrames(state.seiType);
            const config = mp4.getConfig();

            // Build time -> SEI map
            let timeMs = 0;
            const seiEntries = [];
            
            for (let i = 0; i < frames.length; i++) {
                if (frames[i].sei) {
                    seiEntries.push({
                        timeMs,
                        sei: frames[i].sei
                    });
                }
                timeMs += config.durations[i] || 33;
            }

            // Update segment duration from parsed data (more accurate than video metadata)
            const parsedDuration = timeMs;
            if (parsedDuration > 0 && Math.abs(state.segmentDurations[index] - parsedDuration) > 500) {
                console.log(`Segment ${index}: SEI parsed duration ${parsedDuration}ms (was ${state.segmentDurations[index]}ms)`);
                state.segmentDurations[index] = parsedDuration;
                recalculateTotalDuration();
                updateScrubberRange();
            }

            state.seiData[index] = seiEntries;
            console.log(`Parsed ${seiEntries.length} SEI entries for segment ${index}, duration: ${formatTime(parsedDuration / 1000)}`);
        } catch (err) {
            console.error(`Failed to parse SEI for segment ${index}:`, err);
            state.seiData[index] = [];
        }
    }

    function recalculateTotalDuration() {
        state.totalDuration = state.segmentDurations.reduce((sum, d) => sum + (d || 0), 0);
        // Update timeline to reflect new durations
        renderSegmentTimeline();
    }

    function revokeObjectUrls() {
        // Revoke all object URLs (for all cameras, not just active ones)
        Object.keys(state.objectUrls).forEach(cam => {
            URL.revokeObjectURL(state.objectUrls[cam]);
            delete state.objectUrls[cam];
        });
    }

    // =========================================================================
    // PLAYBACK
    // =========================================================================

    function togglePlayPause() {
        if (state.playing) {
            pause();
        } else {
            play();
        }
    }

    function play() {
        state.playing = true;
        elements.playIcon.style.display = 'none';
        elements.pauseIcon.style.display = 'block';

        // Play all active camera videos at current rate
        for (const cam of state.activeCameras) {
            if (videos[cam]) {
                videos[cam].playbackRate = state.playbackRate;
                videos[cam].play().catch(() => {});
            }
        }
    }

    function pause() {
        state.playing = false;
        elements.playIcon.style.display = 'block';
        elements.pauseIcon.style.display = 'none';
        
        // Pause all active camera videos
        for (const cam of state.activeCameras) {
            videos[cam]?.pause();
        }
    }

    function handleVideoLoaded() {
        // Update duration when video metadata is loaded with actual duration
        const masterCam = state.activeCameras.includes('front') ? 'front' : state.activeCameras[0];
        if (!masterCam || !videos[masterCam]) return;
        
        const duration = videos[masterCam].duration;
        if (duration && !isNaN(duration)) {
            const actualDurationMs = duration * 1000;
            const currentIndex = state.currentSegmentIndex;
            
            // Only update if significantly different from estimate (avoids constant recalculation)
            const diff = Math.abs(state.segmentDurations[currentIndex] - actualDurationMs);
            if (diff > 500) { // More than 0.5s difference
                console.log(`Segment ${currentIndex}: updated duration from ${state.segmentDurations[currentIndex]}ms to ${actualDurationMs}ms`);
                state.segmentDurations[currentIndex] = actualDurationMs;
                recalculateTotalDuration();
                updateScrubberRange();
            }
        }
    }

    function handleTimeUpdate() {
        // Use front camera as master if available, otherwise first active camera
        const masterCam = state.activeCameras.includes('front') ? 'front' : state.activeCameras[0];
        if (!masterCam || !videos[masterCam]) return;
        
        const masterTime = videos[masterCam].currentTime;
        
        // Sync other videos
        for (const cam of state.activeCameras) {
            if (cam !== masterCam && videos[cam]?.src) {
                const diff = Math.abs(videos[cam].currentTime - masterTime);
                if (diff > 0.1) {
                    videos[cam].currentTime = masterTime;
                }
            }
        }

        // Update scrubber
        const globalTime = getGlobalTime();
        elements.scrubber.value = globalTime;
        elements.timeDisplay.textContent = `${formatTime(globalTime / 1000)} / ${formatTime(state.totalDuration / 1000)}`;

        // Update SEI display
        updateSeiDisplay(globalTime);
    }

    function handleSegmentEnd() {
        if (state.currentSegmentIndex < state.segments.length - 1) {
            console.log(`Segment ${state.currentSegmentIndex + 1} ended, loading next segment`);
            const wasPlaying = state.playing;
            
            // Load next segment
            loadSegment(state.currentSegmentIndex + 1).then(() => {
                // Seek to start of new segment
                for (const cam of state.activeCameras) {
                    if (videos[cam]?.src) {
                        videos[cam].currentTime = 0;
                    }
                }
                
                if (wasPlaying) {
                    play();
                }
            });
        } else {
            // End of all segments
            console.log('All segments finished');
            pause();
            
            // Reset to beginning
            elements.scrubber.value = 0;
            elements.timeDisplay.textContent = `0:00 / ${formatTime(state.totalDuration / 1000)}`;
        }
    }

    function handleScrub() {
        const globalTime = parseFloat(elements.scrubber.value);
        seekToGlobalTime(globalTime);
    }

    function getGlobalTime() {
        let offset = 0;
        for (let i = 0; i < state.currentSegmentIndex; i++) {
            offset += state.segmentDurations[i] || 0;
        }
        // Use front camera as master if available, otherwise first active camera
        const masterCam = state.activeCameras.includes('front') ? 'front' : state.activeCameras[0];
        const currentTime = masterCam && videos[masterCam] ? videos[masterCam].currentTime : 0;
        return offset + (currentTime || 0) * 1000;
    }

    async function seekToGlobalTime(globalTimeMs) {
        // Clamp to valid range
        globalTimeMs = Math.max(0, Math.min(globalTimeMs, state.totalDuration));
        
        // Find which segment this time falls into
        let offset = 0;
        let targetSegment = 0;
        
        for (let i = 0; i < state.segments.length; i++) {
            const segDuration = state.segmentDurations[i];
            if (offset + segDuration > globalTimeMs) {
                targetSegment = i;
                break;
            }
            offset += segDuration;
            if (i === state.segments.length - 1) {
                targetSegment = i;
                // Clamp offset to not exceed segment start
                offset = state.totalDuration - segDuration;
            }
        }

        const localTime = Math.max(0, (globalTimeMs - offset) / 1000);

        if (targetSegment !== state.currentSegmentIndex) {
            const wasPlaying = state.playing;
            pause(); // Pause during segment switch
            
            await loadSegment(targetSegment);
            
            // Wait for a video to be ready
            const masterCam = state.activeCameras.includes('front') ? 'front' : state.activeCameras[0];
            if (masterCam && videos[masterCam]) {
                await new Promise(resolve => {
                    if (videos[masterCam].readyState >= 1) {
                        resolve();
                    } else {
                        videos[masterCam].addEventListener('loadedmetadata', resolve, { once: true });
                    }
                });
            }

            // Seek all active camera videos
            for (const cam of state.activeCameras) {
                if (videos[cam]?.src && videos[cam].readyState >= 1) {
                    videos[cam].currentTime = Math.min(localTime, videos[cam].duration || localTime);
                }
            }

            if (wasPlaying) {
                play();
            }
        } else {
            // Same segment, just seek
            for (const cam of state.activeCameras) {
                if (videos[cam]?.src) {
                    videos[cam].currentTime = Math.min(localTime, videos[cam].duration || localTime);
                }
            }
        }

        // Update scrubber position
        elements.scrubber.value = globalTimeMs;
        
        // Update display
        elements.timeDisplay.textContent = `${formatTime(globalTimeMs / 1000)} / ${formatTime(state.totalDuration / 1000)}`;
        updateSeiDisplay(globalTimeMs);
    }

    function updateScrubberRange() {
        elements.scrubber.max = state.totalDuration || 100;
        updateTrimVisual();
    }

    // =========================================================================
    // SEI DISPLAY
    // =========================================================================

    function updateSeiDisplay(globalTimeMs) {
        // Calculate which segment and local time
        let offset = 0;
        let segmentIndex = state.currentSegmentIndex;
        
        for (let i = 0; i < state.currentSegmentIndex; i++) {
            offset += state.segmentDurations[i] || 0;
        }

        const localTimeMs = Math.max(0, globalTimeMs - offset);
        const seiEntries = state.seiData[segmentIndex];
        
        // Handle case where SEI data hasn't been parsed yet
        if (!seiEntries || seiEntries.length === 0) {
            renderSei(null);
            return;
        }
        
        // Binary search for current SEI (find last entry with timeMs <= localTimeMs)
        let sei = null;
        let left = 0;
        let right = seiEntries.length - 1;
        
        while (left <= right) {
            const mid = Math.floor((left + right) / 2);
            if (seiEntries[mid].timeMs <= localTimeMs) {
                sei = seiEntries[mid].sei;
                left = mid + 1;
            } else {
                right = mid - 1;
            }
        }

        renderSei(sei);
    }

    // HUD icon markup. Steering wheel = Material Design Icons (Apache-2.0);
    // pedal = Qlementine Icons (MIT), used for both brake and accelerator.
    const HUD_ICONS = {
        wheel: '<svg viewBox="0 0 24 24"><path fill="currentColor" d="M13,19.92C14.8,19.7 16.35,18.95 17.65,17.65C18.95,16.35 19.7,14.8 19.92,13H16.92C16.7,14 16.24,14.84 15.54,15.54C14.84,16.24 14,16.7 13,16.92V19.92M10,8H14L17,11H19.92C19.67,9.05 18.79,7.38 17.27,6C15.76,4.66 14,4 12,4C10,4 8.24,4.66 6.73,6C5.21,7.38 4.33,9.05 4.08,11H7L10,8M11,19.92V16.92C10,16.7 9.16,16.24 8.46,15.54C7.76,14.84 7.3,14 7.08,13H4.08C4.3,14.77 5.05,16.3 6.35,17.6C7.65,18.9 9.2,19.67 11,19.92M12,2C14.75,2 17.1,3 19.05,4.95C21,6.9 22,9.25 22,12C22,14.75 21,17.1 19.05,19.05C17.1,21 14.75,22 12,22C9.25,22 6.9,21 4.95,19.05C3,17.1 2,14.75 2,12C2,9.25 3,6.9 4.95,4.95C6.9,3 9.25,2 12,2Z"/></svg>',
        pedal: '<svg viewBox="0 0 24 24"><path fill="currentColor" fill-rule="evenodd" clip-rule="evenodd" d="M4.44 2.18c-.436.856-.436 1.98-.436 4.22v4.65l.025-.025c.548-.548 1.24-.794 2.12-.911C7 10 8.09 10 9.47 10h5.08c1.38 0 2.47 0 3.32.114c.872.117 1.57.363 2.12.91l.025.026V6.4c0-2.24 0-3.36-.436-4.22a4.03 4.03 0 0 0-1.75-1.75c-.856-.436-1.98-.436-4.22-.436h-3.2c-2.24 0-3.36 0-4.22.436a4.03 4.03 0 0 0-1.75 1.75zM8 5A2 2 0 1 0 8.001.999A2 2 0 0 0 8 5m10-2a2 2 0 1 1-4.001.001A2 2 0 0 1 18 3m-6 6a2 2 0 1 0 .001-4.001A2 2 0 0 0 12 9"/><path fill="currentColor" fill-rule="evenodd" clip-rule="evenodd" d="M20 16.5c0-1.43-.001-2.45-.105-3.23c-.102-.762-.296-1.21-.627-1.54s-.781-.524-1.54-.627c-.776-.104-1.8-.105-3.23-.105h-5c-1.43 0-2.45.001-3.23.105c-.762.102-1.21.296-1.54.627s-.524.781-.627 1.54c-.104.776-.105 1.8-.105 3.23v1.1c0 2.24 0 3.36.436 4.22c.384.753.995 1.36 1.75 1.75c.856.436 1.98.436 4.22.436h3.2c2.24 0 3.36 0 4.22-.436a4.03 4.03 0 0 0 1.75-1.75c.436-.856.436-1.98.436-4.22v-1.1zM6 17c0-.932 0-1.4.152-1.77c.203-.49.592-.879 1.08-1.08c.368-.152.833-.152 1.77-.152h6c.932 0 1.4 0 1.77.152c.49.203.879.592 1.08 1.08c.152.368.152.834.152 1.77s0 1.4-.152 1.77c-.203.49-.592.879-1.08 1.08c-.368.152-.834.152-1.77.152h-6c-.932 0-1.4 0-1.77-.152a2 2 0 0 1-1.08-1.08C6 18.402 6 17.936 6 17"/></svg>',
        arrowL: '<svg viewBox="0 0 24 24"><polygon points="15,5 7,12 15,19" fill="currentColor"/></svg>',
        arrowR: '<svg viewBox="0 0 24 24"><polygon points="9,5 17,12 9,19" fill="currentColor"/></svg>'
    };

    let hudEls = null;
    function buildHud() {
        elements.seiContent.innerHTML = `
            <div class="hud-pill">
                <div class="hud-col hud-left">
                    <div class="hud-gear" data-el="gear"></div>
                    <div class="hud-icon hud-brake" data-el="brake" title="Brake">${HUD_ICONS.pedal}</div>
                </div>
                <div class="hud-col hud-center">
                    <div class="hud-speed-row">
                        <span class="hud-arrow left" data-el="arrowL">${HUD_ICONS.arrowL}</span>
                        <span class="hud-speed-num" data-el="speed">0</span>
                        <span class="hud-arrow right" data-el="arrowR">${HUD_ICONS.arrowR}</span>
                    </div>
                    <div class="hud-unit" data-el="unit"></div>
                    <div class="hud-autopilot" data-el="ap"></div>
                </div>
                <div class="hud-col hud-right">
                    <div class="hud-icon hud-wheel" data-el="wheel" title="Manual">${HUD_ICONS.wheel}</div>
                    <div class="hud-icon hud-accel" data-el="accel" title="Accelerator">${HUD_ICONS.pedal}</div>
                </div>
            </div>
        `;
        const q = (n) => elements.seiContent.querySelector('[data-el="' + n + '"]');
        hudEls = {
            gear: q('gear'), brake: q('brake'), arrowL: q('arrowL'), speed: q('speed'),
            arrowR: q('arrowR'), unit: q('unit'), ap: q('ap'), wheel: q('wheel'), accel: q('accel')
        };
    }

    function renderSei(sei) {
        const hud = elements.seiHud || document.getElementById('seiHud');

        // No data for the current frame: fade the HUD out and hide the map
        if (!sei) {
            hud?.classList.remove('visible');
            updateMap(null, null, null);
            return;
        }

        // Build the pill DOM once, then only mutate values below — this keeps the
        // blinker CSS animation running instead of restarting it every frame.
        if (!hudEls || !elements.seiContent.querySelector('.hud-pill')) buildHud();

        const speedMps = Math.abs(sei.vehicleSpeedMps || 0);
        const speedNum = state.useMetric ? Math.round(speedMps * 3.6) : Math.round(speedMps * 2.237);
        const unitLabel = state.useMetric ? 'KM/H' : 'MPH';

        const autopilotStates = ['', 'Self Driving', 'Autosteer', 'TACC'];
        const apIndex = sei.autopilotState || 0;
        const apActive = apIndex > 0;
        const apLabel = autopilotStates[apIndex] || '';

        const gearLetter = ['P', 'D', 'R', 'N'][sei.gearState || 0] || 'P';
        const acceleratorPct = Math.max(0, Math.min(100, sei.acceleratorPedalPosition || 0));
        const accelActive = acceleratorPct > 2;
        const brakeActive = !!sei.brakeApplied;
        const leftOn = !!sei.blinkerOnLeft;
        const rightOn = !!sei.blinkerOnRight;

        hudEls.gear.textContent = gearLetter;
        hudEls.gear.className = 'hud-gear gear-' + gearLetter;
        hudEls.speed.textContent = speedNum;
        hudEls.unit.textContent = unitLabel;
        hudEls.ap.textContent = apLabel;
        hudEls.ap.classList.toggle('active', apActive);
        hudEls.brake.classList.toggle('active', brakeActive);
        hudEls.accel.classList.toggle('active', accelActive);
        hudEls.accel.title = 'Accelerator ' + Math.round(acceleratorPct) + '%';
        hudEls.wheel.classList.toggle('active', apActive);
        hudEls.wheel.title = apActive ? apLabel : 'Manual';
        hudEls.arrowL.classList.toggle('active', leftOn);
        hudEls.arrowR.classList.toggle('active', rightOn);

        hud?.classList.add('visible');

        // Update map with location and heading
        updateMap(sei.latitudeDeg, sei.longitudeDeg, sei.headingDeg);
    }

    // =========================================================================
    // MAP
    // =========================================================================

    function initMap() {
        // Check if Leaflet is available
        if (typeof L === 'undefined') {
            console.warn('Leaflet not loaded, map functionality disabled');
            return;
        }

        const mapElement = document.getElementById('map');
        if (!mapElement) return;

        // Initialize map centered on a default location
        state.map = L.map('map', {
            center: [0, 0],
            zoom: 17,
            zoomControl: true,
            attributionControl: true
        });

        // Define tile layers
        state.mapLayers = {
            // CARTO dark basemap, recoloured via CSS into a GTA-style minimap
            street: L.tileLayer('https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}.png', {
                attribution: '© OpenStreetMap © CARTO',
                subdomains: 'abcd',
                maxZoom: 20
            }),
            satellite: L.tileLayer('https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}', {
                attribution: '© Esri',
                maxZoom: 19
            })
        };

        // Add default layer
        state.mapLayers.street.addTo(state.map);
        state.currentLayer = 'street';
        document.querySelector('.map-wrapper')?.classList.add('layer-street');
        state.mapFollowHeading = false; // Default: face north
        state.lastHeading = 0;

        // Create car marker with arrow SVG
        state.carMarker = null;
        state.carMarkerAdded = false;

        // Setup layer toggle buttons
        document.querySelectorAll('.map-layer-btn').forEach(btn => {
            btn.addEventListener('click', () => setMapLayer(btn.dataset.layer));
        });

        // Setup orientation toggle buttons
        document.querySelectorAll('.map-orient-btn').forEach(btn => {
            btn.addEventListener('click', () => setMapOrientation(btn.dataset.orient));
        });

        console.log('Map initialized');
    }

    function createCarMarker(heading) {
        // Create a div icon with the car arrow
        const rotation = heading || 0;
        const carIcon = L.divIcon({
            className: 'car-marker-container',
            html: `<div class="car-arrow" style="transform: rotate(${rotation}deg)">
                <svg viewBox="0 0 32 32" width="32" height="32">
                    <polygon points="16,4 8,28 16,22 24,28" fill="#3b9bff" stroke="#ffffff" stroke-width="2" stroke-linejoin="round"/>
                </svg>
            </div>`,
            iconSize: [32, 32],
            iconAnchor: [16, 16]
        });
        return carIcon;
    }

    function setMapLayer(layer) {
        if (!state.map || !state.mapLayers[layer]) return;

        // Remove current layer
        state.map.eachLayer(l => {
            if (l instanceof L.TileLayer) {
                state.map.removeLayer(l);
            }
        });

        // Add new layer
        state.mapLayers[layer].addTo(state.map);
        state.currentLayer = layer;

        // The GTA recolour filter applies only to the street basemap
        const wrapper = document.querySelector('.map-wrapper');
        wrapper?.classList.toggle('layer-street', layer === 'street');
        wrapper?.classList.toggle('layer-satellite', layer === 'satellite');

        // Update button states
        document.querySelectorAll('.map-layer-btn').forEach(btn => {
            btn.classList.toggle('active', btn.dataset.layer === layer);
        });
    }

    function setMapOrientation(orient) {
        state.mapFollowHeading = (orient === 'heading');

        // Update button states
        document.querySelectorAll('.map-orient-btn').forEach(btn => {
            btn.classList.toggle('active', btn.dataset.orient === orient);
        });

        // Apply rotation based on mode
        applyMapRotation(state.lastHeading || 0);
    }

    function applyMapRotation(heading) {
        const mapElement = document.getElementById('map');
        const compassIndicator = document.getElementById('compassIndicator');
        const controlContainer = mapElement?.querySelector('.leaflet-control-container');

        if (state.mapFollowHeading) {
            // Follow mode: rotate map so car heading points up
            // Map rotates opposite to heading
            const rotation = -(heading || 0);
            
            if (mapElement) {
                mapElement.style.transform = `rotate(${rotation}deg)`;
            }
            
            // Counter-rotate controls so they stay upright
            if (controlContainer) {
                controlContainer.style.transform = `rotate(${-rotation}deg)`;
            }
            
            // Compass shows where north is (rotates with heading)
            if (compassIndicator) {
                compassIndicator.style.transform = `translateX(-50%) rotate(${rotation}deg)`;
            }
        } else {
            // North mode: map stays oriented north
            if (mapElement) {
                mapElement.style.transform = 'rotate(0deg)';
            }
            if (controlContainer) {
                controlContainer.style.transform = 'rotate(0deg)';
            }
            if (compassIndicator) {
                compassIndicator.style.transform = 'translateX(-50%) rotate(0deg)';
            }
        }
    }

    function updateMap(lat, lng, heading) {
        if (!state.map) return;

        const mapPlaceholder = document.getElementById('mapPlaceholder');

        // Check for valid coordinates
        if (lat == null || lng == null || (lat === 0 && lng === 0)) {
            mapPlaceholder?.classList.remove('hidden');
            if (state.carMarker && state.carMarkerAdded) {
                state.map.removeLayer(state.carMarker);
                state.carMarkerAdded = false;
            }
            return;
        }

        mapPlaceholder?.classList.add('hidden');
        state.lastHeading = heading || 0;

        const position = [lat, lng];

        // Recreate the marker at the new position/heading
        if (state.carMarker) {
            state.map.removeLayer(state.carMarker);
        }
        state.carMarker = L.marker(position, { icon: createCarMarker(heading || 0) });
        state.carMarker.addTo(state.map);
        state.carMarkerAdded = true;

        // Apply map rotation based on mode
        applyMapRotation(heading);

        // Pan to marker
        state.map.setView(position, state.map.getZoom(), { animate: true, duration: 0.25 });
    }

    // =========================================================================
    // SETTINGS
    // =========================================================================

    function setUnit(unit) {
        state.useMetric = unit === 'metric';
        elements.unitBtns.forEach(btn => {
            btn.classList.toggle('active', btn.dataset.unit === unit);
        });
        // Refresh display
        updateSeiDisplay(getGlobalTime());
    }

    function setPlaybackRate(rate) {
        state.playbackRate = rate;
        for (const cam of state.activeCameras) {
            if (videos[cam]) {
                videos[cam].playbackRate = rate;
            }
        }
        document.querySelectorAll('.speed-btn').forEach(btn => {
            btn.classList.toggle('active', parseFloat(btn.dataset.rate) === rate);
        });
    }

    function toggleShortcutsModal() {
        document.getElementById('shortcutsModal')?.classList.toggle('hidden');
    }

    // =========================================================================
    // KEYBOARD SHORTCUTS
    // =========================================================================

    function handleKeydown(e) {
        // Ignore if typing in input
        if (e.target.tagName === 'INPUT') return;

        switch (e.key) {
            case ' ':
                e.preventDefault();
                togglePlayPause();
                break;
            case 'ArrowLeft':
                e.preventDefault();
                if (e.shiftKey) {
                    seekToGlobalTime(Math.max(0, getGlobalTime() - 30000));
                } else {
                    seekToGlobalTime(Math.max(0, getGlobalTime() - 5000));
                }
                break;
            case 'ArrowRight':
                e.preventDefault();
                if (e.shiftKey) {
                    seekToGlobalTime(Math.min(state.totalDuration, getGlobalTime() + 30000));
                } else {
                    seekToGlobalTime(Math.min(state.totalDuration, getGlobalTime() + 5000));
                }
                break;
            case ',':
                // Step back one frame (~33ms) when paused
                if (!state.playing) {
                    e.preventDefault();
                    seekToGlobalTime(Math.max(0, getGlobalTime() - 33));
                }
                break;
            case '.':
                // Step forward one frame (~33ms) when paused
                if (!state.playing) {
                    e.preventDefault();
                    seekToGlobalTime(Math.min(state.totalDuration, getGlobalTime() + 33));
                }
                break;
            case '[':
            case 'PageUp':
                e.preventDefault();
                jumpToSegment(state.currentSegmentIndex - 1);
                break;
            case ']':
            case 'PageDown':
                e.preventDefault();
                jumpToSegment(state.currentSegmentIndex + 1);
                break;
            case 'Home':
                e.preventDefault();
                seekToGlobalTime(0);
                break;
            case 'End':
                e.preventDefault();
                seekToGlobalTime(state.totalDuration - 1000);
                break;
            case 'i':
            case 'I':
                e.preventDefault();
                setTrimIn();
                break;
            case 'o':
            case 'O':
                e.preventDefault();
                setTrimOut();
                break;
            case '?':
                e.preventDefault();
                toggleShortcutsModal();
                break;
            case 'Escape':
                document.getElementById('shortcutsModal')?.classList.add('hidden');
                break;
        }
    }

    // =========================================================================
    // UTILITIES
    // =========================================================================

    function formatTime(seconds) {
        if (isNaN(seconds) || seconds < 0) seconds = 0;
        
        const hours = Math.floor(seconds / 3600);
        const mins = Math.floor((seconds % 3600) / 60);
        const secs = Math.floor(seconds % 60);
        
        if (hours > 0) {
            return `${hours}:${mins.toString().padStart(2, '0')}:${secs.toString().padStart(2, '0')}`;
        }
        return `${mins}:${secs.toString().padStart(2, '0')}`;
    }

    // =========================================================================
    // TRIM & EXPORT
    // =========================================================================

    function toggleExportPanel() {
        state.exportPanelOpen = !state.exportPanelOpen;
        document.getElementById('exportBar')?.classList.toggle('hidden', !state.exportPanelOpen);
        document.getElementById('exportToggleBtn')?.classList.toggle('active', state.exportPanelOpen);
        if (state.exportPanelOpen) {
            updateTrimDisplay();
        } else {
            // Clear scrubber highlight when panel is closed
            elements.scrubber.style.background = '';
        }
    }

    function setTrimIn() {
        state.trimIn = getGlobalTime();
        // If out point is now before in point, clear it
        if (state.trimOut !== null && state.trimOut <= state.trimIn) {
            state.trimOut = null;
        }
        updateTrimDisplay();
    }

    function setTrimOut() {
        const t = getGlobalTime();
        if (t <= state.trimIn) return; // silently ignore bad selection
        state.trimOut = t;
        updateTrimDisplay();
    }

    function updateTrimDisplay() {
        const outMs = state.trimOut ?? state.totalDuration;
        const inEl = document.getElementById('trimInDisplay');
        const outEl = document.getElementById('trimOutDisplay');
        const durEl = document.getElementById('trimDurDisplay');

        if (inEl) {
            inEl.textContent = formatTime(state.trimIn / 1000);
            inEl.classList.toggle('is-set', state.trimIn > 0);
        }
        if (outEl) {
            outEl.textContent = formatTime(outMs / 1000);
            outEl.classList.toggle('is-set', state.trimOut !== null);
        }
        if (durEl) {
            durEl.textContent = formatTime((outMs - state.trimIn) / 1000);
        }
        updateTrimVisual();
    }

    function updateTrimVisual() {
        if (!state.exportPanelOpen || !state.totalDuration) {
            elements.scrubber.style.background = '';
            return;
        }
        const inPct  = (state.trimIn / state.totalDuration) * 100;
        const outMs  = state.trimOut ?? state.totalDuration;
        const outPct = (outMs  / state.totalDuration) * 100;

        elements.scrubber.style.background =
            `linear-gradient(to right,
                var(--border) 0%,
                var(--border) ${inPct}%,
                rgba(34,197,94,0.45) ${inPct}%,
                rgba(34,197,94,0.45) ${outPct}%,
                var(--border) ${outPct}%,
                var(--border) 100%)`;
    }

    async function startExport() {
        if (state.isExporting) return;
        if (state.segments.length === 0) return;

        const trimOutMs = state.trimOut ?? state.totalDuration;
        const trimDuration = trimOutMs - state.trimIn;
        if (trimDuration <= 0) {
            alert('Set valid In and Out points before exporting.');
            return;
        }

        const [exportW, exportH] = (document.getElementById('exportResSelect')?.value || '1920x1080')
            .split('x').map(Number);

        state.isExporting = true;

        const progressSection = document.getElementById('exportProgress');
        const progressFill    = document.getElementById('exportProgressFill');
        const progressLabel   = document.getElementById('exportProgressLabel');
        const exportBtn       = document.getElementById('exportBtn');

        progressSection?.classList.remove('hidden');
        if (exportBtn) exportBtn.disabled = true;

        // Seek to trim start
        pause();
        await seekToGlobalTime(state.trimIn);
        await new Promise(r => setTimeout(r, 300)); // let decoders settle

        // Build canvas
        const canvas = document.createElement('canvas');
        canvas.width  = exportW;
        canvas.height = exportH;
        const ctx = canvas.getContext('2d');

        // Choose best available codec
        const mimeType = ['video/webm;codecs=vp9', 'video/webm;codecs=vp8', 'video/webm']
            .find(t => MediaRecorder.isTypeSupported(t)) || 'video/webm';

        const stream   = canvas.captureStream(30);
        const recorder = new MediaRecorder(stream, { mimeType, videoBitsPerSecond: 10_000_000 });
        const chunks   = [];

        recorder.ondataavailable = e => { if (e.data.size > 0) chunks.push(e.data); };

        let cancelled = false;
        state.exportCancelFn = () => { cancelled = true; };

        recorder.onstop = () => {
            state.isExporting = false;
            state.exportCancelFn = null;
            progressSection?.classList.add('hidden');
            if (progressFill) progressFill.style.width = '0%';
            if (exportBtn) exportBtn.disabled = false;

            if (!cancelled && chunks.length > 0) {
                const blob = new Blob(chunks, { type: 'video/webm' });
                const url  = URL.createObjectURL(blob);
                const a    = document.createElement('a');
                a.href     = url;
                a.download = `teslacam-${formatTime(state.trimIn / 1000).replace(/:/g, '-')}.webm`;
                a.click();
                setTimeout(() => URL.revokeObjectURL(url), 10000);
            }
        };

        recorder.start(200); // collect data every 200 ms

        // Start playback at 1× (export always real-time)
        for (const cam of state.activeCameras) {
            if (videos[cam]) videos[cam].playbackRate = 1;
        }
        play();

        function drawFrame() {
            if (cancelled) {
                recorder.stop();
                pause();
                return;
            }

            const globalNow = getGlobalTime();

            // Stop when we've reached the out point
            if (globalNow >= trimOutMs) {
                // Seek all videos right to the out frame so the last frame is clean
                recorder.stop();
                pause();
                return;
            }

            // Black fill
            ctx.fillStyle = '#000';
            ctx.fillRect(0, 0, exportW, exportH);

            // Draw each camera widget proportionally to its GridStack position
            const gridItems = state.grid ? state.grid.getGridItems() : [];
            for (const item of gridItems) {
                const node  = item.gridstackNode;
                if (!node) continue;
                const video = videos[node.id];
                if (!video || !video.videoWidth) continue;

                const dx = (node.x / 12) * exportW;
                const dy = (node.y / 6)  * exportH;
                const dw = (node.w / 12) * exportW;
                const dh = (node.h / 6)  * exportH;

                drawVideoContained(ctx, video, dx, dy, dw, dh);
            }

            // Progress
            const elapsed = Math.max(0, globalNow - state.trimIn);
            const pct = Math.min(100, (elapsed / trimDuration) * 100);
            if (progressFill) progressFill.style.width = pct + '%';
            if (progressLabel) {
                progressLabel.textContent =
                    `Exporting ${formatTime(elapsed / 1000)} / ${formatTime(trimDuration / 1000)}`;
            }

            requestAnimationFrame(drawFrame);
        }

        requestAnimationFrame(drawFrame);
    }

    // Object-fit: contain — draw video letterboxed inside a destination rect
    function drawVideoContained(ctx, video, dx, dy, dw, dh) {
        const vr = video.videoWidth  / video.videoHeight;
        const dr = dw / dh;
        let sx, sy, sw, sh;
        if (vr > dr) {
            sw = dw; sh = sw / vr;
            sx = dx; sy = dy + (dh - sh) / 2;
        } else {
            sh = dh; sw = sh * vr;
            sx = dx + (dw - sw) / 2; sy = dy;
        }
        ctx.drawImage(video, sx, sy, sw, sh);
    }

    function cancelExport() {
        if (state.exportCancelFn) state.exportCancelFn();
    }

    // =========================================================================
    // DEBUG: programmatic file loader (for testing without file picker)
    // =========================================================================
    window.__testLoad = async function(fileDescriptors) {
        console.log(`[testLoad] Fetching ${fileDescriptors.length} files...`);
        const files = await Promise.all(fileDescriptors.map(async ({ url, name }) => {
            const resp = await fetch(url);
            if (!resp.ok) throw new Error(`Failed to fetch ${name}: ${resp.status}`);
            const blob = await resp.blob();
            return new File([blob], name, { type: 'video/mp4' });
        }));
        console.log(`[testLoad] Loaded ${files.length} files, building segments...`);
        elements.dropOverlay.classList.add('hidden');
        pause();
        revokeObjectUrls();
        await buildSegments(files);
        if (state.segments.length > 0) {
            await loadSegment(0);
            elements.playBtn.disabled = false;
            elements.scrubber.disabled = false;
        }
        console.log(`[testLoad] Done — ${state.segments.length} segments ready.`);
    };

    // Start the app
    init();

    // DEBUG: expose internal state for diagnostics
    window.__otcState = () => state;
})();
