import { useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

type Entry = { path: string; name: string; extension: string | null; size: number; modified: number | null; category: string };
type DuplicateFile = { path: string; size: number; modified: number | null };
type DuplicateGroup = { sha256: string; files: DuplicateFile[] };
type DuplicateProgress = { stage: string; scanned: number; total: number };
type DuplicateScanResult = { groups: DuplicateGroup[]; skipped: number };
type PlanItem = { source: string; target: string; action: string; size: number; status: string; error: string | null };
type Plan = { id: string; items: PlanItem[]; status: string; created: number };
type CollectionResult = { folder: string; items: { source: string; target: string; status: string; error: string | null }[]; job_id: string | null };
type ScreenRegion = { x: number; y: number; width: number; height: number };
type RegionPair = { icons: ScreenRegion; files: ScreenRegion };
type SceneRect = { x: number; y: number; width: number; height: number };
type DesktopScene = { display: SceneRect; work: SceneRect; wallpaper: string | null; icons: string | null; widgets: { name: string; bounds: SceneRect; image: string | null }[]; permission: boolean };
type SpacingLevel = 'auto' | 'xs' | 's' | 'm' | 'l' | 'xl';
type IconAlign = 'left' | 'center' | 'right';
type RegionKey = 'icons' | 'files';
type RegionLayout = { spacing: SpacingLevel; align: IconAlign };
type PlannedIcon = { kind: 'app' | 'folder' | 'file'; x: number; y: number };
type DesktopArrangeSummary = { apps: number; folders: number; files: number; total: number; failed: string[] };
type ScanEvent = { kind: 'item' | 'done' | 'error' | 'warning' | 'cancelled'; item?: Entry; scanned?: number; message?: string };
const categories = ['전체', '문서', '사진', '영상', '압축', '기타'];
const helpText = '폴더를 선택하면 파일 목록을 읽어 옵니다. 파일 이동·이름 변경은 계획을 미리 보고 승인한 뒤에만 실행되며, 작업 이력에서 되돌릴 수 있습니다. 바탕화면 정렬은 위의 정리 범위에서 실행합니다. ⌘K로 파일 검색에 바로 갈 수 있습니다.';
const sleep = (ms: number) => new Promise(resolve => window.setTimeout(resolve, ms));
const defaultRegions: RegionPair = { icons: { x: .035, y: .08, width: .43, height: .84 }, files: { x: .535, y: .08, width: .43, height: .84 } };
function loadRegions(): RegionPair {
  try {
    const value = JSON.parse(localStorage.getItem('cleaner-desktop-regions') ?? 'null') as RegionPair | null;
    if (value && value.icons && value.files && [value.icons, value.files].every(r => [r.x,r.y,r.width,r.height].every(Number.isFinite) && r.x >= 0 && r.y >= 0 && r.width >= .12 && r.height >= .12 && r.x+r.width <= 1 && r.y+r.height <= 1)) return value;
  } catch { /* use the initial screen regions */ }
  return defaultRegions;
}
// Converts a rectangle in screen points to a fraction of the desktop work area (the editor's coordinate space).
function toWorkFraction(rect: SceneRect, work: SceneRect): ScreenRegion { return { x: (rect.x-work.x)/work.width, y: (rect.y-work.y)/work.height, width: rect.width/work.width, height: rect.height/work.height }; }
// Each step is a horizontal × vertical gap in points, chosen so desktop icons and their
// labels never touch even at the tightest step.
const spacingLevels: { level: SpacingLevel; label: string; gap: { x: number; y: number } | null }[] = [
  { level: 'auto', label: '자동', gap: null },
  { level: 'xs', label: '아주 좁게', gap: { x: 84, y: 88 } },
  { level: 's', label: '좁게', gap: { x: 96, y: 100 } },
  { level: 'm', label: '보통', gap: { x: 112, y: 114 } },
  { level: 'l', label: '넓게', gap: { x: 132, y: 132 } },
  { level: 'xl', label: '아주 넓게', gap: { x: 158, y: 154 } },
];
const alignOptions: { align: IconAlign; label: string }[] = [{ align: 'left', label: '왼쪽' }, { align: 'center', label: '가운데' }, { align: 'right', label: '오른쪽' }];
function loadAlign(): IconAlign {
  try { const value = localStorage.getItem('cleaner-icon-align'); if (value === 'left' || value === 'center' || value === 'right') return value; } catch { /* use the default */ }
  return 'center';
}
function loadSpacing(): SpacingLevel {
  try {
    const value = localStorage.getItem('cleaner-icon-spacing-level');
    if (spacingLevels.some(option => option.level === value)) return value as SpacingLevel;
  } catch { /* use the default */ }
  return 'auto';
}
// Each region (apps / folders·files) keeps its own spacing and alignment.
function loadLayouts(): Record<RegionKey, RegionLayout> {
  const valid = (value: unknown): value is RegionLayout => typeof value === 'object' && value !== null
    && spacingLevels.some(option => option.level === (value as RegionLayout).spacing) && alignOptions.some(option => option.align === (value as RegionLayout).align);
  try {
    const value = JSON.parse(localStorage.getItem('cleaner-region-layouts') ?? 'null') as Record<RegionKey, RegionLayout> | null;
    if (value && valid(value.icons) && valid(value.files)) return value;
  } catch { /* fall back below */ }
  const shared = { spacing: loadSpacing(), align: loadAlign() }; // carry over the earlier single setting
  return { icons: shared, files: { ...shared } };
}
function styleArg(layout: RegionLayout) { return { spacing: spacingLevels.find(option => option.level === layout.spacing)?.gap ?? null, align: layout.align }; }
// The extension stays fixed while renaming so a copy never loses its file type.
function splitFileName(path: string) {
  const base = path.split(/[\\/]/).pop() ?? path;
  const compound = base.match(/\.tar\.(gz|bz2|xz|zst)$/i);
  const dot = compound && compound.index ? compound.index : base.lastIndexOf('.');
  return dot > 0 ? { stem: base.slice(0, dot), ext: base.slice(dot) } : { stem: base, ext: '' };
}
function fileNameProblem(name: string) {
  if (!name.trim()) return '이름을 입력해 주세요';
  if (name.trim().startsWith('.')) return '점(.)으로 시작할 수 없어요';
  if (/[\/\\:<>"|?*]/.test(name) || /[\u0000-\u001f]/.test(name)) return '/ \\ : < > " | ? * 기호는 쓸 수 없어요';
  if (/[. ]$/.test(name)) return '점이나 공백으로 끝날 수 없어요';
  if (/^(con|prn|aux|nul|com[1-9]|lpt[1-9])( *\.|\s*$)/i.test(name)) return 'Windows가 예약한 이름이에요';
  if (new TextEncoder().encode(name.trim()).length > 255) return '이름이 너무 길어요';
  return '';
}
function overlaps(a: ScreenRegion,b: ScreenRegion) { return a.x < b.x+b.width && a.x+a.width > b.x && a.y < b.y+b.height && a.y+a.height > b.y; }

export default function App() {
  const [entries, setEntries] = useState<Entry[]>([]), [root, setRoot] = useState('');
  const [query, setQuery] = useState(''), [filter, setFilter] = useState('전체');
  const [busy, setBusy] = useState(false), [scanned, setScanned] = useState(0), [error, setError] = useState('');
  const [view, setView] = useState<'files' | 'duplicates' | 'analysis' | 'desktop' | 'rules'>('files');
  const [duplicates, setDuplicates] = useState<DuplicateGroup[]>([]), [duplicateBusy, setDuplicateBusy] = useState(false);
  const [duplicateProgress, setDuplicateProgress] = useState<DuplicateProgress>({ stage: 'hash', scanned: 0, total: 0 });
  const [representatives, setRepresentatives] = useState<Record<string, string>>({}), [limit, setLimit] = useState(200);
  const [planning, setPlanning] = useState<Entry | null>(null), [targetFolder, setTargetFolder] = useState(''), [targetName, setTargetName] = useState(''), [planPreview, setPlanPreview] = useState<Plan | null>(null), [operationBusy, setOperationBusy] = useState(false);
  const [history, setHistory] = useState<Plan[]>([]), [historyVisible, setHistoryVisible] = useState(false);
  const [watchEnabled, setWatchEnabled] = useState(false), [newCandidates, setNewCandidates] = useState<string[]>([]);
  const [lastWatchScan, setLastWatchScan] = useState<number | null>(null), [watchScanning, setWatchScanning] = useState(false);
  const [selectedForFolder, setSelectedForFolder] = useState<string[]>([]), [folderName, setFolderName] = useState('모은 파일'), [collectionResult, setCollectionResult] = useState<CollectionResult | null>(null);
  const [fileStems, setFileStems] = useState<string[]>([]), [bulkBase, setBulkBase] = useState('');
  const [moveTarget, setMoveTarget] = useState<'new' | 'existing'>('new'), [existingFolder, setExistingFolder] = useState(''), [collectionUndo, setCollectionUndo] = useState('');
  const finalNames = selectedForFolder.map((path, index) => (fileStems[index] ?? '').trim() + splitFileName(path).ext);
  // APFS is case-insensitive, so "A.txt" and "a.txt" would collide.
  const nameProblems = finalNames.map((name, index) => fileNameProblem(fileStems[index] ?? '') || (finalNames.findIndex(other => other.toLowerCase() === name.toLowerCase()) !== index ? '위에 같은 이름이 있어요' : ''));
  const [notice, setNotice] = useState(''), [collectProgress, setCollectProgress] = useState<{ done: number; total: number; name: string } | null>(null);
  const [regions,setRegions] = useState<RegionPair>(loadRegions), [regionsOpen,setRegionsOpen] = useState(true), [screenSize,setScreenSize] = useState({width:16,height:10});
  const [scene,setScene] = useState<DesktopScene | null>(null), [sceneError,setSceneError] = useState(''), [sceneLoading,setSceneLoading] = useState(false);
  const sceneLoadedAt = useRef(0), sceneBusy = useRef(false), pickerAt = useRef(0);
  const [layouts,setLayouts] = useState<Record<RegionKey, RegionLayout>>(loadLayouts), [planned,setPlanned] = useState<PlannedIcon[]>([]), [planNote,setPlanNote] = useState('');
  const updateLayout = (key: RegionKey, change: Partial<RegionLayout>) => setLayouts(value => ({ ...value, [key]: { ...value[key], ...change } }));
  const [ruleText, setRuleText] = useState(() => localStorage.getItem('moa-extension-rules') ?? '');
  const [filenameTemplate, setFilenameTemplate] = useState(() => localStorage.getItem('moa-filename-template') ?? '');
  const [layoutPreference, setLayoutPreference] = useState(() => localStorage.getItem('moa-layout-preference') ?? 'categories');
  // watchBaseline: paths that already existed when watching began (never reported as new).
  // silentScan: the current scan is a background watcher pass whose result replaces the list once, at the end.
  const watchEnabledRef = useRef(false), watchBaseline = useRef<Set<string> | null>(null), watchPending = useRef<Map<string,string>>(new Map()), watchCollected = useRef<Entry[]>([]), replaceNextScan = useRef(false), watcherRunning = useRef(false), silentScan = useRef(false);
  const busyRef = useRef(false), duplicateRun = useRef(0), searchInput = useRef<HTMLInputElement | null>(null);
  const pendingEntries = useRef<Entry[]>([]), pendingScanned = useRef(0);
  useEffect(() => { busyRef.current = busy; }, [busy]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => { if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') { event.preventDefault(); focusSearch(); } };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);
  useEffect(() => { invoke<{width:number;height:number}>('desktop_screen_info').then(size => { if (size.width > 0 && size.height > 0) setScreenSize(size); }).catch(() => {}); }, []);
  async function refreshScene(iconsOnly = false) {
    if (sceneBusy.current) return;
    sceneBusy.current = true; setSceneLoading(true); setSceneError('');
    try {
      const fresh = await invoke<DesktopScene>('desktop_scene', { iconsOnly });
      // An icons-only refresh keeps the wallpaper and widget images already on screen.
      const next = iconsOnly ? { ...fresh, wallpaper: scene?.wallpaper ?? fresh.wallpaper, widgets: fresh.widgets.map((widget, index) => ({ ...widget, image: widget.image ?? scene?.widgets[index]?.image ?? null })) } : fresh;
      setScene(next); sceneLoadedAt.current = Date.now();
    }
    catch (e) { setSceneError(String(e)); }
    finally { sceneBusy.current = false; setSceneLoading(false); }
  }
  async function allowScreenCapture() {
    try { if (await invoke<boolean>('request_screen_capture_permission')) { await refreshScene(); return; } } catch {}
    setSceneError('시스템 설정 > 개인정보 보호 및 보안 > 화면 및 시스템 오디오 녹음에서 tidy를 켠 뒤, 앱을 완전히 종료하고 다시 열어 주세요.');
  }
  // Load the real desktop on start, and again when returning to the app (the desktop may have changed meanwhile).
  useEffect(() => {
    // Launching or switching to the app animates a Space change; wait it out before capturing.
    let timer = window.setTimeout(() => void refreshScene(), 400);
    // Closing a file picker refocuses the window; that is not the user coming back from the desktop.
    const onFocus = () => { if (Date.now() - pickerAt.current > 2000 && Date.now() - sceneLoadedAt.current > 15000) { window.clearTimeout(timer); timer = window.setTimeout(() => void refreshScene(), 900); } };
    window.addEventListener('focus', onFocus);
    return () => { window.clearTimeout(timer); window.removeEventListener('focus', onFocus); };
  }, []);
  const widgetRegions = useMemo(() => scene ? scene.widgets.map(widget => ({ name: widget.name, region: toWorkFraction(widget.bounds, scene.work) })) : [], [scene]);
  const coveredWidgets = widgetRegions.filter(widget => overlaps(widget.region, regions.icons) || overlaps(widget.region, regions.files)).map(widget => widget.name);
  useEffect(() => { localStorage.setItem('cleaner-desktop-regions',JSON.stringify(regions)); }, [regions]);
  useEffect(() => { try { localStorage.setItem('cleaner-region-layouts', JSON.stringify(layouts)); } catch { /* per-viewer convenience only */ } }, [layouts]);
  // Show where every desktop item would land with the current regions and spacing (nothing moves).
  useEffect(() => {
    if (!regionsOpen || overlaps(regions.icons, regions.files)) { setPlanned([]); setPlanNote(''); return; }
    let stale = false;
    const timer = window.setTimeout(() => {
      invoke<PlannedIcon[]>('preview_desktop_arrangement', { iconArea: regions.icons, fileArea: regions.files, iconStyle: styleArg(layouts.icons), fileStyle: styleArg(layouts.files) })
        .then(icons => { if (!stale) { setPlanned(icons); setPlanNote(''); } })
        .catch(e => { if (!stale) { setPlanned([]); setPlanNote(String(e)); } });
    }, 450);
    return () => { stale = true; window.clearTimeout(timer); };
  }, [regions, layouts, regionsOpen, scene]);
  const overrides = useMemo(() => new Map(ruleText.split('\n').map(line => line.split('=')).filter(parts => parts.length === 2).map(([extension, category]) => [extension.trim().replace(/^\./,'').toLowerCase(), category.trim()])), [ruleText]);
  const categorized = useMemo(() => entries.map(entry => ({...entry,category: overrides.get(entry.extension ?? '') ?? entry.category})), [entries, overrides]);
  // Custom categories from the user's rules get their own filter tab.
  const allCategories = useMemo(() => [...new Set([...categories, ...overrides.values()].filter(Boolean))], [overrides]);
  const visible = useMemo(() => categorized.filter(e => (filter === '전체' || e.category === filter) && e.name.toLocaleLowerCase().includes(query.toLocaleLowerCase())), [categorized, filter, query]);
  function flushPending() {
    if (pendingEntries.current.length || replaceNextScan.current) { const batch = pendingEntries.current; pendingEntries.current = []; setEntries(prev => replaceNextScan.current ? (replaceNextScan.current = false, batch) : prev.concat(batch)); setScanned(pendingScanned.current); }
  }
  useEffect(() => {
    // listen() resolves asynchronously; if cleanup already ran (StrictMode remount), drop the late listener right away.
    let disposed = false; const unlisteners: (() => void)[] = [];
    const keep = (fn: () => void) => { if (disposed) fn(); else unlisteners.push(fn); };
    listen<ScanEvent>('scan-event', ({ payload }) => {
    if (payload.kind === 'item' && payload.item) {
      if (watchEnabledRef.current || silentScan.current) watchCollected.current.push(payload.item);
      if (!silentScan.current) { pendingEntries.current.push(payload.item); pendingScanned.current = payload.scanned ?? 0; }
    }
    if (payload.kind === 'done') {
      const silent = silentScan.current; silentScan.current = false;
      const current = silent || watchCollected.current.length ? watchCollected.current : pendingEntries.current;
      if (watchEnabledRef.current && watchBaseline.current) {
        const baseline = watchBaseline.current, next = new Map(current.map(item => [item.path, `${item.size}:${item.modified ?? ''}`]));
        const candidates: string[] = [];
        for (const item of current) {
          if (baseline.has(item.path)) continue;
          if (/\.(part|tmp|crdownload|download)$/i.test(item.name) || item.name.startsWith('~$')) continue;
          // A new file becomes a candidate once two consecutive scans see the same size/mtime.
          const fingerprint = next.get(item.path)!;
          if (watchPending.current.get(item.path) === fingerprint) { candidates.push(item.path); watchPending.current.delete(item.path); baseline.add(item.path); }
          else watchPending.current.set(item.path, fingerprint);
        }
        for (const path of [...watchPending.current.keys()]) if (!next.has(path)) watchPending.current.delete(path);
        setNewCandidates(prev => [...new Set([...prev, ...candidates])].filter(path => next.has(path)));
      }
      watchCollected.current = [];
      if (watchEnabledRef.current) setLastWatchScan(Date.now());
      if (silent) { setEntries(current); setScanned(payload.scanned ?? 0); }
      else { pendingScanned.current = payload.scanned ?? 0; flushPending(); setScanned(payload.scanned ?? 0); setBusy(false); }
    }
    if (payload.kind === 'error') {
      watchCollected.current = [];
      if (silentScan.current) { silentScan.current = false; setError(`폴더 감지 스캔 오류: ${payload.message ?? '스캔에 실패했습니다.'}`); return; }
      flushPending(); replaceNextScan.current = false; setError(payload.message ?? '스캔에 실패했습니다.'); setBusy(false);
    }
    if (payload.kind === 'warning' && !silentScan.current) setError(payload.message ?? '일부 파일을 읽지 못했습니다.');
    if (payload.kind === 'cancelled') {
      watchCollected.current = [];
      if (silentScan.current) { silentScan.current = false; return; }
      pendingEntries.current = []; setEntries([]); setScanned(0); setError('스캔을 취소했습니다. 결과는 저장하지 않았습니다.'); setBusy(false);
    }
  }).then(keep);
    listen<DuplicateProgress>('duplicate-progress', ({ payload }) => setDuplicateProgress(payload)).then(keep);
    listen<string>('duplicate-warning', ({ payload }) => setError(payload)).then(keep);
    listen<{ done: number; total: number; name: string }>('collect-progress', ({ payload }) => setCollectProgress(payload)).then(keep);
    const batchTimer = window.setInterval(flushPending, 150);
    return () => { disposed = true; window.clearInterval(batchTimer); unlisteners.splice(0).forEach(fn => fn()); };
  }, []);
  useEffect(() => {
    watchEnabledRef.current = watchEnabled;
    if (!watchEnabled || !root) { watchBaseline.current = null; watchPending.current.clear(); return; }
    watchBaseline.current = new Set(entries.map(item => item.path));
    const timer = window.setInterval(async () => {
      if (watcherRunning.current || !watchEnabledRef.current || busyRef.current) return;
      watcherRunning.current = true; silentScan.current = true; watchCollected.current = []; setWatchScanning(true);
      try { await invoke('scan_folder', { path: root }); } catch (e) { silentScan.current = false; setError(`폴더 감지 스캔 오류: ${String(e)}`); }
      finally { watcherRunning.current = false; setWatchScanning(false); }
    }, 30000);
    return () => { window.clearInterval(timer); watchBaseline.current = null; };
  }, [watchEnabled, root]);
  async function chooseAndScan() {
    pickerAt.current = Date.now();
    const selected = await invoke<string | null>('pick_scan_folder'); pickerAt.current = Date.now();
    if (typeof selected !== 'string') return;
    if (duplicateBusy) { duplicateRun.current++; void invoke('cancel_duplicate_scan'); }
    await waitForWatcher();
    pendingEntries.current = []; pendingScanned.current = 0;
    watchEnabledRef.current = false; watchBaseline.current = null; setWatchEnabled(false); setNewCandidates([]); setRoot(selected); setEntries([]); setScanned(0); setError(''); setBusy(true); setView('files'); setLimit(200); setDuplicates([]);
    try {
      await invoke('scan_folder', { path: selected });
    } catch (e) { setError(String(e)); } finally { setBusy(false); }
  }
  async function arrangeDesktop() {
    setError(''); setNotice(''); setOperationBusy(true);
    if (overlaps(regions.icons,regions.files)) { setError('앱 아이콘 범위와 폴더·파일 범위가 겹칩니다. 미리보기에서 두 범위를 분리해 주세요.'); setOperationBusy(false); return; }
    let activeRegions=regions, expandedLabel='';
    try {
      let result: DesktopArrangeSummary;
      try { result=await invoke<DesktopArrangeSummary>('arrange_desktop_in_regions',{iconArea:activeRegions.icons,fileArea:activeRegions.files,iconStyle:styleArg(layouts.icons),fileStyle:styleArg(layouts.files)}); }
      catch (firstError) {
        const detail=String(firstError);
        const fit=detail.match(/(앱 아이콘|폴더·파일) 범위에 항목 (\d+)개.*?현재 (\d+)×(\d+)pt, 권장 최소 (\d+)×(\d+)pt/);
        if (!fit) throw firstError;
        const size=await invoke<{width:number;height:number}>('desktop_screen_info');
        const key=fit[1] === '폴더·파일' ? 'files' : 'icons';
        const region=activeRegions[key], other=activeRegions[key === 'files' ? 'icons' : 'files'];
        const neededWidth=Number(fit[5]), neededHeight=Number(fit[6]);
        // Judge "can't fit" by the real requirement; the 4% headroom is only a nicety.
        if (neededWidth > size.width || neededHeight > size.height) throw new Error(`${fit[1]} 범위를 화면 끝까지 넓혀도 항목 ${fit[2]}개가 들어가지 않습니다. 아이콘 간격을 줄이거나 파일 일부를 폴더로 옮겨 주세요.`);
        const requiredWidth=Math.min(1,Math.max(region.width,neededWidth/size.width*1.04));
        const requiredHeight=Math.min(1,Math.max(region.height,neededHeight/size.height*1.04));
        const right=region.x+region.width;
        const nextRegion={...region,width:requiredWidth,height:requiredHeight};
        // Grow away from the screen edge instead of past it (a range flush with the right edge used to fail here).
        nextRegion.x=key === 'icons' ? Math.max(0,right-nextRegion.width) : Math.max(0,Math.min(region.x,1-nextRegion.width));
        nextRegion.y=Math.min(region.y,Math.max(0,1-nextRegion.height));
        if (overlaps(nextRegion,other)) throw new Error(`${fit[1]} 범위를 넓히면 다른 범위와 겹칩니다. 미리보기에서 두 범위 사이를 띄우거나 아이콘 간격을 줄여 주세요.`);
        activeRegions={...activeRegions,[key]:nextRegion};
        setRegions(activeRegions); localStorage.setItem('cleaner-desktop-regions',JSON.stringify(activeRegions));
        expandedLabel=key === 'icons' ? '앱 아이콘 범위를 항목 수에 맞게 넓혀 ' : '폴더·파일 범위를 항목 수에 맞게 넓혀 ';
        result=await invoke<DesktopArrangeSummary>('arrange_desktop_in_regions',{iconArea:activeRegions.icons,fileArea:activeRegions.files,iconStyle:styleArg(layouts.icons),fileStyle:styleArg(layouts.files)});
      }
      setNotice(`${expandedLabel}정렬 완료 · 앱 ${result.apps}개 · 폴더 ${result.folders}개 · 파일 ${result.files}개 (${result.total}개)`);
      void refreshScene(true);
      if (result.failed.length) setError(`${result.failed.length}개 항목은 탐색기가 옮기지 못해 그대로 두었습니다: ${result.failed.slice(0, 5).join(', ')}${result.failed.length > 5 ? ' 외' : ''}`);
    }
    catch (e) { setError(String(e)); }
    finally { setOperationBusy(false); }
  }
  async function chooseFilesForNewFolder() {
    setError(''); setCollectionResult(null);
    pickerAt.current = Date.now();
    try { const files=await invoke<string[] | null>('pick_files_for_collection'); pickerAt.current = Date.now(); if (files) { setSelectedForFolder(files); setFileStems(files.map(file => splitFileName(file).stem)); setBulkBase(''); setFolderName('모은 파일'); } }
    catch (e) { setError(String(e)); }
  }
  async function chooseExistingFolder() {
    pickerAt.current = Date.now();
    try { const path = await invoke<string | null>('pick_move_destination'); pickerAt.current = Date.now(); if (path) { setExistingFolder(path); setMoveTarget('existing'); } }
    catch (e) { setError(String(e)); }
  }
  async function moveChosenFiles() {
    setOperationBusy(true); setError(''); setCollectionUndo(''); setCollectProgress({ done: 0, total: selectedForFolder.length, name: '' });
    const destination = moveTarget === 'new' ? { kind: 'new', name: folderName } : { kind: 'existing', path: existingFolder };
    try {
      const result=await invoke<CollectionResult>('move_selected_files',{destination,paths:selectedForFolder,names:finalNames}); setCollectionResult(result); setSelectedForFolder([]);
      void refreshScene(true); if (root) void refreshScan();
    }
    catch (e) { setError(String(e)); }
    finally { setOperationBusy(false); setCollectProgress(null); }
  }
  async function undoCollection() {
    if (!collectionResult?.job_id) return;
    setOperationBusy(true); setError('');
    try {
      const result = await invoke<Plan>('undo_operation', { planId: collectionResult.job_id });
      setCollectionUndo(result.status === 'undone' ? '원래 위치로 모두 되돌렸습니다.' : '일부 파일은 되돌리지 못했습니다. 작업 이력에서 사유를 확인해 주세요.');
      void refreshScene(true); if (root) void refreshScan();
    }
    catch (e) { setError(String(e)); }
    finally { setOperationBusy(false); }
  }
  async function findDuplicates() {
    if (!root) return;
    const run = ++duplicateRun.current;
    setError(''); setNotice(''); setDuplicateBusy(true); setDuplicateProgress({ stage: 'hash', scanned: 0, total: 0 }); setView('duplicates'); setRepresentatives({}); setDuplicates([]);
    try { const result = await invoke<DuplicateScanResult>('scan_duplicates', { path: root }); if (run === duplicateRun.current) setDuplicates(result.groups); }
    catch (e) { if (run !== duplicateRun.current) return; const detail = String(e); if (detail.includes('취소')) setNotice('중복 찾기를 취소했습니다.'); else setError(detail); }
    finally { setDuplicateBusy(false); }
  }
  async function cancel() { await invoke('cancel_scan'); }
  async function cancelDuplicates() { await invoke('cancel_duplicate_scan'); }
  // Manual scans wait for a background watcher pass so the two never interleave events.
  async function waitForWatcher() { while (watcherRunning.current) await sleep(100); }
  function focusSearch() { setView('files'); window.setTimeout(() => { searchInput.current?.focus(); searchInput.current?.select(); }, 0); }
  async function saveRules() {
    try { localStorage.setItem('moa-extension-rules', ruleText); localStorage.setItem('moa-filename-template',filenameTemplate); localStorage.setItem('moa-layout-preference',layoutPreference); setNotice('규칙을 저장했습니다.'); }
    catch { setError('규칙을 저장하지 못했습니다.'); }
  }
  async function exportRules() {
    try { if (await invoke<boolean>('export_rules', { contents: JSON.stringify({extensionRules:ruleText,filenameTemplate,layoutPreference},null,2) })) setNotice('규칙 파일을 저장했습니다.'); }
    catch (e) { setError(String(e)); }
  }
  function beginPlanning(entry: Entry) {
    const separator = entry.path.includes('\\') ? '\\' : '/';
    setPlanning(entry); setTargetFolder(entry.path.slice(0, entry.path.lastIndexOf(separator))); const parts=entry.name.split('.'); const ext=parts.length>1?parts.pop()!:''; const stem=parts.join('.'); const suggestion=filenameTemplate ? filenameTemplate.split('{name}').join(stem).split('{ext}').join(ext).split('{date}').join(new Date().toISOString().slice(0,10)).replace(/\.+$/, '') : entry.name; setTargetName(suggestion); setPlanPreview(null); setError('');
  }
  async function chooseTargetFolder() {
    if (!root) return;
    pickerAt.current = Date.now();
    try { const path = await invoke<string | null>('pick_destination_folder', { path: root }); pickerAt.current = Date.now(); if (path) setTargetFolder(path); }
    catch (e) { setError(String(e)); }
  }
  async function createPlan() {
    if (!planning || !root || !targetName.trim()) return;
    const separator = planning.path.includes('\\') ? '\\' : '/';
    const target = `${targetFolder}${separator}${targetName.trim()}`;
    try {
      const result = await invoke<Plan>('create_operation_plan', { path: root, operations: [{ source: planning.path, target }] });
      setPlanPreview(result); setPlanning(null);
    } catch (e) { setError(String(e)); }
  }
  async function executePlan() {
    if (!planPreview) return;
    setOperationBusy(true); setError('');
    try {
      const result = await invoke<Plan>('execute_plan', { planId: planPreview.id }); setPlanPreview(result);
      if (result.status === 'succeeded') await refreshScan();
    } catch (e) { setError(String(e)); } finally { setOperationBusy(false); }
  }
  async function undoPlan() {
    if (!planPreview) return;
    setOperationBusy(true); setError('');
    try { const result = await invoke<Plan>('undo_operation', { planId: planPreview.id }); setPlanPreview(result); if (result.status === 'undone') await refreshScan(); }
    catch (e) { setError(String(e)); } finally { setOperationBusy(false); }
  }
  async function showHistory() {
    try { setHistory(await invoke<Plan[]>('list_operations')); setHistoryVisible(value => !value); }
    catch (e) { setError(String(e)); }
  }
  async function undoHistory(id: string) {
    setOperationBusy(true);
    setError('');
    try { const result = await invoke<Plan>('undo_operation', { planId: id }); setHistory(await invoke<Plan[]>('list_operations')); if (result.status !== 'undone') setError('일부 항목은 되돌리지 못했습니다. 작업 이력에서 사유를 확인해 주세요.'); if (root) await refreshScan(); }
    catch (e) { setError(String(e)); } finally { setOperationBusy(false); }
  }
  async function refreshScan() {
    await waitForWatcher();
    replaceNextScan.current = true; pendingEntries.current = []; pendingScanned.current = 0; setEntries([]); setScanned(0); setBusy(true);
    try { await invoke('scan_folder', { path: root }); } catch (e) { setError(String(e)); } finally { setBusy(false); }
  }
  const totalBytes = entries.reduce((n, e) => n + e.size, 0);
  return <main className="shell">
    <aside className="rail"><div className="brand">tid<span>y</span></div><button type="button" className={`rail-icon ${view === 'files' ? 'active' : ''}`} title="파일 목록" aria-label="파일 목록" onClick={() => { setView('files'); window.scrollTo({ top: 0, behavior: 'smooth' }); }}>▦</button><button type="button" className="rail-icon" title="파일 검색 (⌘K)" aria-label="파일 검색" onClick={focusSearch}>⌕</button><button type="button" className="rail-bottom" title="도움말" aria-label="도움말" onClick={() => setNotice(helpText)}>?</button></aside>
    <section className="content">
      <header><div><div className="eyebrow">LOCAL WORKSPACE</div><h1>파일 정리</h1></div><div className="privacy"><i/> 이 기기에서만 처리됩니다</div></header>
      <section className="welcome"><div className="welcome-copy"><div className="eyebrow">내 파일, 한눈에</div><h2>원하는 자리에<br/><em>가지런히 놓으세요.</em></h2><p>앱 아이콘은 앱끼리, 폴더와 파일은 그 외끼리<br/>내가 정한 두 영역에 모아 바탕화면을 정리합니다.</p><button className="primary" onClick={arrangeDesktop} disabled={operationBusy || busy}>{operationBusy ? '배치 중…' : '지정한 범위에 정렬'} <span>↗</span></button><button className="primary" onClick={chooseFilesForNewFolder} disabled={operationBusy || busy}>파일 골라 옮기기 <span>↗</span></button><button className="text-button" onClick={chooseAndScan} disabled={busy || watchScanning}>다른 폴더 살펴보기</button></div><div className="orb"><div className="orb-core">✳</div><div className="float-card fc1">▧ <b>문서</b></div><div className="float-card fc2">◉ <b>사진</b></div><div className="float-card fc3">◫ <b>기타</b></div></div></section>
      <section className="plan-card region-card"><div className="region-heading"><div><h3>정리 범위</h3><p>지금 내 PC의 배경화면 위에 아이콘이 놓일 자리를 미리 보여 줍니다. 실제 아이콘 이미지는 표시되지 않습니다.</p></div><button className="text-button" onClick={() => setRegionsOpen(value => !value)}>{regionsOpen ? '접기' : '범위 편집'}</button></div>{regionsOpen && <><DesktopRegionEditor regions={regions} onChange={setRegions} aspectRatio={scene ? scene.work.width/scene.work.height : screenSize.width/screenSize.height} scene={scene} widgets={widgetRegions} loading={sceneLoading} planned={planned}/><div className="layout-panels">{(['icons','files'] as const).map(key => { const layout=layouts[key]; const name=key === 'icons' ? '앱 아이콘' : '폴더·파일'; return <div className="layout-panel" key={key}><b><i className={key === 'icons' ? 'legend-icon' : 'legend-files'}/>{name}</b><div className="layout-row"><span>간격</span><div className="spacing-steps" role="radiogroup" aria-label={`${name} 간격`}>{spacingLevels.map(option => <button key={option.level} type="button" role="radio" aria-checked={layout.spacing === option.level} className={layout.spacing === option.level ? 'selected' : ''} title={option.gap ? undefined : '범위 크기에 맞춰 고르게 퍼뜨려요'} onClick={() => updateLayout(key, { spacing: option.level })}>{option.label}</button>)}</div></div><div className="layout-row"><span>정렬</span><div className="spacing-steps" role="radiogroup" aria-label={`${name} 정렬 위치`}>{alignOptions.map(option => <button key={option.align} type="button" role="radio" aria-checked={layout.align === option.align} className={layout.align === option.align ? 'selected' : ''} onClick={() => updateLayout(key, { align: option.align })}>{option.label}</button>)}</div></div><small>{layout.spacing === 'auto' ? '범위 크기에 맞춰 고르게 퍼뜨리고' : '고른 간격으로 위쪽부터 채우고'}, {layout.align === 'left' ? '왼쪽에 붙여요' : layout.align === 'right' ? '오른쪽에 붙여요' : '가운데에 맞춰요'}</small></div>; })}</div><div className="region-legend"><span><i className="legend-icon"/>앱 아이콘</span><span><i className="legend-files"/>폴더·파일</span><span><i className="legend-planned"/>정렬 후 위치</span><button className="text-button" onClick={() => void refreshScene()} disabled={sceneLoading}>{sceneLoading ? '불러오는 중…' : '현재 화면 다시 불러오기'}</button><button className="text-button" onClick={() => setRegions(defaultRegions)}>기본 범위로</button></div>{scene && !scene.permission && <small className="region-warning">배경화면과 아이콘을 보려면 화면 기록 권한이 필요해요. 위젯 위치는 권한 없이도 표시됩니다. <button className="text-button" onClick={() => void allowScreenCapture()}>권한 허용하기</button></small>}{sceneError && <small className="region-warning">{sceneError}</small>}{planNote && <small className="region-warning">{planNote}</small>}{coveredWidgets.length > 0 && <small className="region-note">범위 안의 위젯 {coveredWidgets.length}개 자리는 비워 두고 아이콘을 배치해요.</small>}{overlaps(regions.icons,regions.files) && <small className="region-warning">두 범위가 겹쳐 있어요. 정렬하려면 겹치지 않게 옮겨주세요.</small>}</>}</section>
      {selectedForFolder.length > 0 && <div className="plan-card"><h3>선택한 파일 옮기기</h3><p>선택한 파일을 원래 위치에서 옮깁니다. 같은 이름이 있으면 덮어쓰지 않고 번호를 붙이며, 작업 이력에서 원래 위치로 되돌릴 수 있습니다.</p><div className="move-target" role="radiogroup" aria-label="옮길 곳"><label className={moveTarget === 'new' ? 'selected' : ''}><input type="radio" name="move-target" checked={moveTarget === 'new'} onChange={() => setMoveTarget('new')}/><span>바탕화면에 새 폴더 만들기</span>{moveTarget === 'new' && <input className="move-folder-name" aria-label="새 폴더 이름" value={folderName} onChange={event => setFolderName(event.target.value)}/>}</label><label className={moveTarget === 'existing' ? 'selected' : ''}><input type="radio" name="move-target" checked={moveTarget === 'existing'} onChange={() => { setMoveTarget('existing'); if (!existingFolder) void chooseExistingFolder(); }}/><span>이미 있는 폴더로 옮기기</span>{moveTarget === 'existing' && <><span className="move-folder-path" title={existingFolder}>{existingFolder || '아직 선택 안 함'}</span><button type="button" className="text-button" onClick={() => void chooseExistingFolder()}>{existingFolder ? '다른 폴더' : '폴더 선택'}</button></>}</label></div>{selectedForFolder.length > 1 && <div className="rename-bulk"><span>이름 한 번에 바꾸기</span><input placeholder="여행" value={bulkBase} onChange={event => setBulkBase(event.target.value)}/><button className="text-button" disabled={!bulkBase.trim()} onClick={() => { const width = String(selectedForFolder.length).length; setFileStems(selectedForFolder.map((_, index) => `${bulkBase.trim()} ${String(index + 1).padStart(width, '0')}`)); }}>번호 붙여 적용</button><button className="text-button" onClick={() => setFileStems(selectedForFolder.map(path => splitFileName(path).stem))}>원래 이름으로</button></div>}<div className="rename-list">{selectedForFolder.map((path, index) => { const { stem, ext } = splitFileName(path); const problem = nameProblems[index]; return <div className="rename-row" key={path}><span className="rename-original" title={path}>{stem + ext}</span><span className="rename-arrow" aria-hidden="true">→</span><div className={`rename-input ${problem ? 'invalid' : ''}`}><input aria-label={`${stem + ext}의 새 이름`} value={fileStems[index] ?? ''} onChange={event => { const value = event.target.value; setFileStems(stems => stems.map((item, i) => i === index ? value : item)); }}/>{ext && <span className="rename-ext">{ext}</span>}</div>{problem && <small className="rename-problem">{problem}</small>}</div>; })}</div><div className="plan-actions"><button className="text-button" onClick={() => setSelectedForFolder([])}>취소</button><button className="primary" onClick={moveChosenFiles} disabled={operationBusy || nameProblems.some(Boolean) || (moveTarget === 'existing' && !existingFolder)}>{collectProgress ? '옮기는 중…' : '선택한 파일 옮기기'}</button></div>{collectProgress && <div className="progress-wrap"><div className="progress determinate"><span style={{width:`${Math.round(collectProgress.done / Math.max(1, collectProgress.total) * 100)}%`}}/></div><small>{collectProgress.done + 1 > collectProgress.total ? collectProgress.total : collectProgress.done + 1} / {collectProgress.total}개 옮기는 중{collectProgress.name ? ` · ${collectProgress.name}` : ''}</small></div>}</div>}
      {collectionResult && <div className="plan-card"><h3>파일 옮기기 결과</h3><p>{collectionResult.folder ? `폴더: ${collectionResult.folder}` : '옮긴 파일이 없어 새 폴더를 남기지 않았습니다.'}</p>{collectionResult.items.map(item => <p key={`${item.source}-${item.target}`}>{item.status === 'moved' ? '이동 완료' : item.status === 'skipped' ? '그대로 둠' : '실패'} · {item.status === 'moved' ? item.target : item.source}{item.error ? ` · ${item.error}` : ''}</p>)}{collectionResult.job_id && <div className="plan-actions">{collectionUndo ? <small>{collectionUndo}</small> : <button className="text-button" onClick={() => void undoCollection()} disabled={operationBusy}>원래 위치로 되돌리기</button>}</div>}</div>}
      <section className="summary"><div><span className="metric-label">현재 폴더</span><strong className="folder-name">{root ? root.split(/[\\/]/).filter(Boolean).slice(-1)[0] : '아직 선택 안 함'}</strong></div><div><span className="metric-label">찾은 파일</span><strong>{entries.length.toLocaleString()}<small>개</small></strong></div><div><span className="metric-label">전체 크기</span><strong>{formatSize(totalBytes)}</strong></div><div className="summary-action">{busy ? <button className="text-button" onClick={cancel}>스캔 취소 <span className="spinner"/></button> : <button className="text-button" onClick={chooseAndScan} disabled={watchScanning}>다른 폴더 선택 ↗</button>}</div></section>
      {busy && <div className="progress-wrap"><div className="progress"><span/></div><small>{scanned.toLocaleString()}개 파일 확인 · 취소하려면 버튼을 누르세요</small></div>}
      {error && <div className="error">{error}</div>}
      {notice && <div role="status" className="plan-card">{notice}</div>}
      <section className="library"><div className="library-head"><div><div className="eyebrow">LIBRARY</div><h2>{view === 'files' ? '파일 둘러보기' : view === 'duplicates' ? '완전히 같은 파일' : view === 'analysis' ? '로컬 콘텐츠 분석' : view === 'desktop' ? '데스크톱 정돈 추천' : '내 규칙과 감지'} <span>{view === 'files' ? entries.length : view === 'duplicates' ? duplicates.length : ''}</span></h2></div><div className="library-actions"><button className="text-button" onClick={() => setView('analysis')}>AI 상태</button><button className="text-button" onClick={() => setView('desktop')}>정돈 추천</button><button className="text-button" onClick={() => setView('rules')}>규칙·감지</button><button className="text-button" onClick={showHistory}>작업 이력</button>{view === 'duplicates' && <button className="text-button" onClick={() => setView('files')}>파일 목록</button>}{view === 'files' && <><button className="text-button duplicate-action" onClick={findDuplicates} disabled={!root || busy || duplicateBusy}>중복 찾기 ⌕</button><label className="search">⌕ <input ref={searchInput} placeholder="파일 이름 검색" value={query} onChange={e => {setQuery(e.target.value);setLimit(200);}}/><kbd>⌘ K</kbd></label></>}</div></div>
        {historyVisible && <div className="history-panel"><h3>작업 이력 <button className="text-button" onClick={() => setHistoryVisible(false)}>닫기</button></h3>{history.length === 0 && <p>저장된 파일 작업이 없습니다.</p>}{history.map(job => <article key={job.id}><div><b>{new Date(job.created * 1000).toLocaleString('ko-KR')}</b><small>{job.status}</small></div>{job.items.map((item,index) => <div className="history-item" key={index}><span>{item.source}</span> → <span>{item.target}</span><small>{item.status}{item.error ? ` · ${item.error}` : ''}</small></div>)}{['succeeded','partial-failure'].includes(job.status) && <button className="text-button" onClick={() => undoHistory(job.id)} disabled={operationBusy}>성공한 항목 되돌리기</button>}</article>)}</div>}
        {view === 'analysis' ? <div className="plan-card"><h3>분석 provider를 사용할 수 없습니다</h3><p>이 버전에는 문서/이미지 분석 모델이나 추출기가 포함되어 있지 않습니다. AI 추천은 생성하지 않으며, 예시 결과도 표시하지 않습니다. 원본 파일 내용은 일반 스캔에서 읽지 않습니다.</p><p>현재 지원: 없음 · 네트워크 분석 호출: 없음 · 분석 기록: 저장하지 않음</p></div> : view === 'desktop' ? <div className="plan-card"><h3>폴더 정리 제안</h3><p>선택 스타일: {layoutPreference}. 아래는 내 규칙을 반영한 파일 종류별 수량을 바탕으로 한 제안입니다. 바탕화면 아이콘 배치는 위의 ‘정리 범위’에서 실행합니다.</p>{root ? allCategories.slice(1).map(category => {const count=categorized.filter(entry=>entry.category===category).length;return <p key={category}>{category}: {count}개 {count ? `— ${category}별 하위 폴더 구성을 검토해 볼 수 있습니다.` : '— 추천 항목 없음'}</p>}) : <p>먼저 스캔할 폴더를 선택하세요.</p>}<small>이 화면은 제안만 표시하며 파일을 옮기지 않습니다 · 바탕화면 아이콘은 주 모니터에서만 정리합니다</small></div> : view === 'rules' ? <div className="plan-card"><h3>내 규칙</h3><p>확장자별 분류를 지정하세요. 한 줄에 확장자=분류를 입력합니다 (예: pdf=문서). 저장 위치는 이 기기의 앱 웹 저장소입니다.</p><textarea aria-label="확장자 분류 규칙" value={ruleText} onChange={event => setRuleText(event.target.value)} rows={4} placeholder="pdf=문서&#10;jpg=사진"/><label>파일명 템플릿<input value={filenameTemplate} onChange={event => setFilenameTemplate(event.target.value)} placeholder="{name}_{date}.{ext}"/></label><small>사용 가능한 값: {'{name}'}, {'{date}'}, {'{ext}'}. 미리보기에서 직접 확인 후 승인해야 적용됩니다.</small><label>레이아웃 선호<select value={layoutPreference} onChange={event => setLayoutPreference(event.target.value)}><option value="minimal">미니멀</option><option value="productivity">생산성</option><option value="categories">카테고리</option><option value="projects">프로젝트</option></select></label><button className="text-button" onClick={() => void saveRules()}>규칙 저장</button><button className="text-button" onClick={() => void exportRules()}>규칙 내보내기</button><button className="text-button" onClick={() => { localStorage.removeItem('moa-extension-rules'); localStorage.removeItem('moa-filename-template'); localStorage.removeItem('moa-layout-preference'); setRuleText(''); setFilenameTemplate(''); setLayoutPreference('categories'); }}>저장 규칙 삭제</button><h3>선택 폴더의 새 파일 감지</h3><p>30초마다 선택 폴더를 다시 읽습니다. 새 파일은 두 번의 스캔에서 크기/수정 시각이 같을 때만 후보로 표시하며 파일은 변경하지 않습니다. 감시는 앱 실행 중에만 유지됩니다.</p><button className={watchEnabled ? 'text-button' : 'primary'} onClick={() => { setWatchEnabled(value => !value); setNewCandidates([]); }} disabled={!root || busy}>{watchEnabled ? '감시 일시 중지' : '이 폴더 감시 시작'}</button><small>{watchEnabled ? `감시 중: ${root} · 마지막 스캔 ${lastWatchScan ? new Date(lastWatchScan).toLocaleTimeString('ko-KR') : '대기 중'} · 재시작 후 꺼짐` : '감시 꺼짐'}</small>{newCandidates.length > 0 && <div><b>정리 후보 ({newCandidates.length})</b>{newCandidates.map(path => <p key={path}>{path}</p>)}<button className="text-button" onClick={() => setNewCandidates([])}>후보 지우기</button></div>}</div> : view === 'files' ? <><nav className="filters">{allCategories.map(c => <button key={c} className={filter === c ? 'selected' : ''} onClick={() => setFilter(c)}>{c}{c === '전체' && entries.length > 0 && <small>{entries.length}</small>}</button>)}</nav>
        <div className="table"><div className="row table-head"><span>이름</span><span>종류</span><span>크기</span><span>수정한 날짜</span><span/></div>
          {visible.slice(0, limit).map((e, i) => <div className="row" key={`${e.path}-${i}`}><span className="file-name"><i className={`file-icon ${e.category}`}>{iconFor(e.category)}</i><span><b>{e.name}</b><small>{e.extension ? `.${e.extension}` : '확장자 없음'}</small></span></span><span className="kind">{e.category}</span><span>{formatSize(e.size)}</span><span>{e.modified ? new Date(e.modified * 1000).toLocaleDateString('ko-KR') : '—'}</span><button className="row-action" onClick={() => beginPlanning(e)}>계획</button></div>)}
          {!busy && visible.length === 0 && <div className="empty"><div>⌕</div><b>{entries.length ? '검색 결과가 없어요' : '아직 표시할 파일이 없어요'}</b><span>{entries.length ? '다른 이름이나 종류로 찾아보세요.' : '위에서 폴더를 선택해 파일을 불러와 보세요.'}</span></div>}
        </div>{visible.length > limit && <button className="load-more" onClick={() => setLimit(n => n + 200)}>파일 더 보기 ({limit.toLocaleString()} / {visible.length.toLocaleString()})</button>}{planning && <div className="plan-card"><h3>작업 계획 만들기</h3><p>원본 파일: <b>{planning.path}</b></p><label>대상 폴더<input value={targetFolder} onChange={e => setTargetFolder(e.target.value)}/><button className="text-button" onClick={chooseTargetFolder}>폴더 선택</button></label><label>새 파일 이름<input value={targetName} onChange={e => setTargetName(e.target.value)}/></label><div className="plan-actions"><button className="text-button" onClick={() => setPlanning(null)}>취소</button><button className="primary" onClick={createPlan}>검토 계획 만들기</button></div></div>}{planPreview && <div className="plan-card"><h3>변경 전 미리보기 · {planPreview.status}</h3>{planPreview.items.map((item,index)=><div className="plan-item" key={`${item.source}-${index}`}><span>{item.action === 'rename' ? '이름 변경' : '이동'}</span><b>{item.source}</b><span>→</span><b>{item.target}</b><small>{item.status}{item.error ? ` · ${item.error}` : ''}</small></div>)}<p>기존 대상 덮어쓰기는 금지됩니다. 운영체제가 안전한 원자적 이동을 지원하지 않으면 작업은 실패합니다.</p><div className="plan-actions">{planPreview.status === 'pending' && <button className="primary" onClick={executePlan} disabled={operationBusy}>이 계획 승인 후 실행</button>}{['succeeded','partial-failure'].includes(planPreview.status) && <button className="text-button" onClick={undoPlan} disabled={operationBusy}>성공 항목 되돌리기</button>}</div></div>}</> : <div className="duplicate-results">
          {duplicateBusy && <div className="progress-wrap duplicate-progress"><div className="progress"><span/></div><small>{duplicateProgress.scanned.toLocaleString()} / {duplicateProgress.total.toLocaleString()}개 · {duplicateProgress.stage === 'hash' ? 'SHA-256 계산' : '바이트 비교'} 중 · 파일 내용은 외부로 보내지 않습니다</small><button className="text-button" onClick={() => void cancelDuplicates()}>중복 찾기 취소</button></div>}
          {!duplicateBusy && duplicates.length === 0 && <div className="empty"><div>⌕</div><b>완전히 같은 파일이 없어요</b><span>크기가 같은 후보만 SHA-256으로 확인했습니다.</span></div>}
          {duplicates.map((group, index) => <article className="duplicate-group" key={`${group.sha256}-${index}`}><header><div><b>중복 그룹 {index + 1}</b><small>{group.files.length}개 파일 · SHA-256 일치 및 바이트 비교</small></div><span>{formatSize(group.files[0].size)}</span></header>{group.files.map(file => <label className="duplicate-file" key={file.path}><input type="radio" name={`representative-${group.sha256}`} checked={representatives[group.sha256] === file.path} onChange={() => setRepresentatives(prev => ({...prev,[group.sha256]:file.path}))}/><span className="path-text">{file.path}</span><small>{file.modified ? new Date(file.modified * 1000).toLocaleDateString('ko-KR') : '—'}</small></label>)}<p>대표 파일을 선택해 표시만 할 수 있습니다. 파일은 이동하거나 삭제되지 않습니다.</p></article>)}
</div>}<footer><span>파일 작업은 미리보기와 사용자 승인 필요</span><span>인덱스는 이 기기에 저장</span></footer>
      </section>
    </section>
  </main>;
}
function formatSize(n: number) { if (n < 1024) return `${n} B`; const units = ['KB','MB','GB','TB']; let v=n/1024, i=0; while(v>=1024&&i<units.length-1){v/=1024;i++;} return `${v.toFixed(1)} ${units[i]}`; }
function iconFor(c: string) { return ({문서:'▤', 사진:'▧', 영상:'▶', 압축:'▣', 기타:'·'} as Record<string,string>)[c] ?? '·'; }

function DesktopRegionEditor({regions,onChange,aspectRatio,scene,widgets,loading,planned}:{regions:RegionPair;onChange:(value:RegionPair)=>void;aspectRatio:number;scene:DesktopScene|null;widgets:{name:string;region:ScreenRegion}[];loading:boolean;planned:PlannedIcon[]}) {
  const canvas=useRef<HTMLDivElement|null>(null);
  const drag=useRef<{key:keyof RegionPair;mode:'move'|'resize';startX:number;startY:number;rect:DOMRect;original:ScreenRegion}|null>(null);
  function begin(event:React.PointerEvent<HTMLElement>,key:keyof RegionPair,mode:'move'|'resize') {
    event.preventDefault(); event.stopPropagation();
    if (!canvas.current) return;
    drag.current={key,mode,startX:event.clientX,startY:event.clientY,rect:canvas.current.getBoundingClientRect(),original:{...regions[key]}};
    event.currentTarget.setPointerCapture(event.pointerId);
  }
  function move(event:React.PointerEvent<HTMLElement>) {
    const active=drag.current; if (!active) return;
    const dx=(event.clientX-active.startX)/active.rect.width, dy=(event.clientY-active.startY)/active.rect.height;
    const original=active.original; let next:ScreenRegion;
    if (active.mode==='move') next={...original,x:Math.min(1-original.width,Math.max(0,original.x+dx)),y:Math.min(1-original.height,Math.max(0,original.y+dy))};
    else next={...original,width:Math.min(1-original.x,Math.max(.12,original.width+dx)),height:Math.min(1-original.y,Math.max(.12,original.height+dy))};
    onChange({...regions,[active.key]:next});
  }
  function end() { drag.current=null; }
  const box=(key:keyof RegionPair,label:string,modifier:string)=>{
    const value=regions[key];
    return <div className={`desktop-zone ${modifier}`} style={{left:`${value.x*100}%`,top:`${value.y*100}%`,width:`${value.width*100}%`,height:`${value.height*100}%`}} onPointerDown={event=>begin(event,key,'move')} onPointerMove={move} onPointerUp={end} onPointerCancel={end} aria-label={label}>
      <b>{label}</b><small>끌어서 이동</small><button type="button" className="zone-resize" title={`${label} 크기 조절`} aria-label={`${label} 크기 조절`} onPointerDown={event=>begin(event,key,'resize')} onPointerMove={move} onPointerUp={end} onPointerCancel={end}>↘</button>
    </div>;
  };
  const ratio=aspectRatio||1.6;
  const place=(region:ScreenRegion)=>({left:`${region.x*100}%`,top:`${region.y*100}%`,width:`${region.width*100}%`,height:`${region.height*100}%`});
  const fullDisplay=scene ? place(toWorkFraction(scene.display,scene.work)) : undefined;
  const real=Boolean(scene?.wallpaper || scene?.icons);
  return <div className="region-editor"><div className={`desktop-map ${real ? 'has-scene' : ''}`} ref={canvas} style={{aspectRatio:ratio,width:`min(100%, ${ratio*390}px)`}} onPointerMove={move} onPointerUp={end} onPointerCancel={end}>
    {scene?.wallpaper && <img className="scene-layer" src={scene.wallpaper} style={fullDisplay} alt="" draggable={false}/>}
    {scene?.icons && <img className="scene-layer" src={scene.icons} style={fullDisplay} alt="" draggable={false}/>}
    {scene?.widgets.map((widget,index) => { const region=widgets[index]?.region; if (!region) return null; return widget.image ? <img key={index} className="scene-widget" src={widget.image} style={place(region)} alt={widget.name} title={`위젯 · ${widget.name}`} draggable={false}/> : <div key={index} className="scene-widget placeholder" style={place(region)} title={`위젯 · ${widget.name}`}><span>{widget.name}</span></div>; })}
    {planned.map((icon,index) => <i key={index} className={`planned-icon planned-${icon.kind}`} style={{left:`${icon.x*100}%`,top:`${icon.y*100}%`}}/>)}
    <span className="map-label">{loading && !scene ? '바탕화면 불러오는 중…' : real ? '실제 바탕화면' : '바탕화면 위치 미리보기'}</span>{box('icons','앱 아이콘','icons-zone')}{box('files','폴더·파일','files-zone')}</div><div className="region-hint">각 영역을 끌어 위치를 옮기고 오른쪽 아래 손잡이로 크기를 바꾸세요.</div></div>;
}
