import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';

import 'bridge/native_bridge.dart';

enum AppSection {
  overview,
  tasks,
  taskCreate,
  taskDetail,
  streams,
  multicast,
  records,
  artifacts,
  uploads,
  nodes,
  security,
}

enum NavigationSource { controller, route }

class ServerProfile {
  const ServerProfile({
    required this.id,
    required this.name,
    required this.baseUrl,
  });

  final String id;
  final String name;
  final String baseUrl;

  Map<String, Object?> toJson() => {
        'id': id,
        'name': name,
        'base_url': baseUrl,
      };

  static ServerProfile fromJson(Map<String, Object?> json) {
    return ServerProfile(
      id: (json['id'] as String?) ?? (json['base_url'] as String?) ?? '',
      name: (json['name'] as String?) ?? (json['base_url'] as String?) ?? '',
      baseUrl: (json['base_url'] as String?) ?? '',
    );
  }
}

class AppController extends ChangeNotifier {
  AppController(this._bridge);

  final NativeBridge _bridge;
  final ChangeNotifier _routerNotifier = ChangeNotifier();

  Listenable get routerListenable => _routerNotifier;

  ServerProfile? server;
  List<ServerProfile> serverProfiles = const [];
  String accessToken = '';
  String refreshToken = '';
  Map<String, Object?>? session;
  AppSection currentSection = AppSection.overview;
  NavigationSource navigationSource = NavigationSource.controller;
  ThemeMode themeMode = ThemeMode.light;
  int autoRefreshSeconds = 5;
  int viewRefreshSeed = 0;
  bool inspectorVisible = true;
  String? inspectorNodeId;
  String? selectedTaskId;
  String? activeMediaUrl;
  String? activeMediaTitle;
  String? errorMessage;
  bool busy = false;
  bool initialized = false;
  File? _localStoreFile;
  Map<String, Object?> _localStore = {};

  bool get isAuthenticated => session != null;
  String get subject => (session?['subject'] as String?) ?? 'unknown';
  String get role => (session?['role'] as String?) ?? 'unknown';
  String get environment => (session?['environment'] as String?) ?? 'desktop';

  @override
  void dispose() {
    _routerNotifier.dispose();
    super.dispose();
  }

  Future<void> initialize() async {
    if (initialized) return;
    busy = true;
    notifyListeners();
    try {
      _loadLocalStore();
      _migrateLegacyLocalStoreIfNeeded();
      _loadPreferences();
      serverProfiles = _readServerProfiles();
      final activeServerId =
          _readLocalStoreString('active_server_id') ?? _firstServerProfileId();
      server = _firstOrNull(
          serverProfiles.where((profile) => profile.id == activeServerId));
      server ??= _firstOrNull(serverProfiles);

      if (server != null) {
        refreshToken = _readStoreValue('refresh_token') ?? '';
      }

      if (server != null && refreshToken.isNotEmpty) {
        try {
          final tokens = await NativeBridge.callOnWorker('auth.refresh', {
            'server': server!.toJson(),
            'refresh_token': refreshToken,
          });
          final previousRefreshToken = refreshToken;
          accessToken = (tokens['access_token'] as String?) ?? '';
          refreshToken = (tokens['refresh_token'] as String?) ?? refreshToken;
          if (refreshToken != previousRefreshToken) {
            _writeStoreValue('refresh_token', refreshToken);
          }
          session = await NativeBridge.callOnWorker('auth.me', {
            'server': server!.toJson(),
            'access_token': accessToken,
          });
        } catch (error) {
          accessToken = '';
          refreshToken = '';
          session = null;
          errorMessage = '登录状态恢复失败：$error';
        }
      }
    } finally {
      initialized = true;
      busy = false;
      _notifyRouter();
      notifyListeners();
    }
  }

  Future<void> login({
    required String baseUrl,
    required String username,
    required String password,
  }) async {
    await _run(() async {
      _activateServerProfile(baseUrl);
      final tokens = await NativeBridge.callOnWorker('auth.login', {
        'server': server!.toJson(),
        'body': {
          'username': username,
          'password': password,
        },
      });
      accessToken = (tokens['access_token'] as String?) ?? '';
      refreshToken = (tokens['refresh_token'] as String?) ?? '';
      if (refreshToken.isNotEmpty) {
        _writeStoreValue('refresh_token', refreshToken);
      }
      session = await NativeBridge.callOnWorker('auth.me', {
        'server': server!.toJson(),
        'access_token': accessToken,
      });
      currentSection = AppSection.overview;
      _notifyRouter();
    });
  }

  Future<void> loginWithoutAuth({
    required String baseUrl,
  }) async {
    await _run(() async {
      _activateServerProfile(baseUrl);
      accessToken = '';
      refreshToken = '';
      _deleteStoreValueIfPresent('refresh_token');
      final me = await NativeBridge.callOnWorker('auth.me', {
        'server': server!.toJson(),
        'access_token': '',
      });
      if (me['auth_enabled'] != false) {
        throw NativeBridgeError('服务端仍启用了鉴权，请切换为账号密码模式。');
      }
      session = me;
      currentSection = AppSection.overview;
      _notifyRouter();
    });
  }

  void selectServer(ServerProfile profile) {
    server = profile;
    _writeLocalStoreValue('active_server_id', profile.id);
    notifyListeners();
  }

  Future<Map<String, Object?>> api(
    String method,
    String path, {
    Map<String, Object?>? query,
    Object? body,
  }) async {
    final active = server;
    if (active == null) {
      throw NativeBridgeError('server is not configured');
    }
    final response = await NativeBridge.callOnWorker('api.request', {
      'server': active.toJson(),
      'access_token': accessToken,
      'refresh_token': refreshToken,
      'method': method,
      'path': path,
      'query': query,
      'body': body,
    });
    final nextAccessToken = response['access_token'] as String?;
    if (nextAccessToken != null && nextAccessToken.isNotEmpty) {
      accessToken = nextAccessToken;
    }
    final nextRefreshToken = response['refresh_token'] as String?;
    if (nextRefreshToken != null &&
        nextRefreshToken.isNotEmpty &&
        nextRefreshToken != refreshToken) {
      refreshToken = nextRefreshToken;
      _writeStoreValue('refresh_token', refreshToken);
    }
    final payload = response['payload'];
    if (payload is Map) return payload.cast<String, Object?>();
    return <String, Object?>{'value': payload};
  }

  Future<List<Map<String, Object?>>> apiList(
    String path, {
    Map<String, Object?>? query,
  }) async {
    final payload = await api('GET', path, query: query);
    final value = payload['items'] ?? payload['value'];
    if (value is List) {
      return value
          .whereType<Map>()
          .map((item) => item.cast<String, Object?>())
          .toList();
    }
    return const [];
  }

  Future<void> mutate(String method, String path, {Object? body}) async {
    await _run(() async {
      await api(method, path, body: body);
    });
  }

  Future<Map<String, Object?>> uploadMedia(String filePath) async {
    final active = server;
    if (active == null) {
      throw NativeBridgeError('server is not configured');
    }
    return NativeBridge.callOnWorker('upload.media', {
      'server': active.toJson(),
      'access_token': accessToken,
      'file_path': filePath,
    });
  }

  void playMedia(String url, {String? title}) {
    if (activeMediaUrl == url && activeMediaTitle == title) {
      return;
    }
    activeMediaUrl = url;
    activeMediaTitle = title;
    notifyListeners();
  }

  void closeMediaPlayer() {
    activeMediaUrl = null;
    activeMediaTitle = null;
    notifyListeners();
  }

  Future<Map<String, Object?>> openExternalMedia(String url) async {
    return NativeBridge.callOnWorker('media_player.open_external', {
      'body': {'url': url},
    });
  }

  Future<Map<String, Object?>> stopMedia(String sessionId) async {
    return NativeBridge.callOnWorker('media_player.stop', {
      'body': {'session_id': sessionId},
    });
  }

  Future<Map<String, Object?>> snapshotMedia(String sessionId,
      {String? outputPath}) async {
    return NativeBridge.callOnWorker('media_player.snapshot', {
      'body': {
        'session_id': sessionId,
        if (outputPath != null && outputPath.isNotEmpty)
          'output_path': outputPath,
      },
    });
  }

  Future<Map<String, Object?>> openMediaProbe() async {
    return NativeBridge.callOnWorker('media_player.probe', {});
  }

  Future<List<Map<String, Object?>>> scanServers({
    List<String> baseUrls = const [],
    List<String> seedHosts = const [],
  }) async {
    final response = await NativeBridge.callOnWorker('server_discovery.scan', {
      'body': {
        'ports': [8080, 80],
        'timeout_ms': 180,
        if (baseUrls.isNotEmpty) 'base_urls': baseUrls,
        if (seedHosts.isNotEmpty) 'seed_hosts': seedHosts,
      },
    });
    final items = response['items'];
    if (items is List) {
      return items
          .whereType<Map>()
          .map((item) => item.cast<String, Object?>())
          .toList();
    }
    return const [];
  }

  Future<Map<String, Object?>> probeServer({
    String? baseUrl,
    String protocol = 'http',
    String host = '',
    int port = 8080,
  }) async {
    return NativeBridge.callOnWorker('server_discovery.probe', {
      'body': {
        if (baseUrl != null && baseUrl.isNotEmpty) 'base_url': baseUrl,
        if (baseUrl == null || baseUrl.isEmpty) ...{
          'protocol': protocol,
          'host': host,
          'port': port,
        },
        'timeout_ms': 1500,
      },
    });
  }

  Future<Map<String, Object?>> diagnostics() async {
    final active = server;
    if (active == null) {
      throw NativeBridgeError('server is not configured');
    }
    return NativeBridge.callOnWorker('diagnostics.probe', {
      'server': active.toJson(),
      'access_token': accessToken,
    });
  }

  void navigate(AppSection section) {
    if (currentSection == section &&
        navigationSource == NavigationSource.controller) {
      return;
    }
    navigationSource = NavigationSource.controller;
    currentSection = section;
    _notifyRouter();
    notifyListeners();
  }

  void openTask(String taskId) {
    if (selectedTaskId == taskId && currentSection == AppSection.taskDetail) {
      return;
    }
    selectedTaskId = taskId;
    navigationSource = NavigationSource.controller;
    currentSection = AppSection.taskDetail;
    _notifyRouter();
    notifyListeners();
  }

  void selectTask(String taskId) {
    if (selectedTaskId == taskId) return;
    selectedTaskId = taskId;
    notifyListeners();
  }

  void syncSectionFromRoute(AppSection section) {
    if (currentSection == section &&
        navigationSource == NavigationSource.route) {
      return;
    }
    currentSection = section;
    navigationSource = NavigationSource.route;
  }

  void setThemeMode(ThemeMode mode) {
    if (themeMode == mode) return;
    themeMode = mode;
    _writeLocalStoreValue('theme_mode', mode.name);
    notifyListeners();
  }

  void toggleThemeMode() {
    setThemeMode(
        themeMode == ThemeMode.dark ? ThemeMode.light : ThemeMode.dark);
  }

  void setAutoRefreshSeconds(int seconds) {
    if (autoRefreshSeconds == seconds) return;
    autoRefreshSeconds = seconds;
    _writeLocalStoreValue('auto_refresh_seconds', seconds);
    notifyListeners();
  }

  void notifyCurrentView() {
    viewRefreshSeed++;
    notifyListeners();
  }

  void setInspectorVisible(bool visible) {
    if (inspectorVisible == visible) return;
    inspectorVisible = visible;
    _writeLocalStoreValue('inspector_visible', visible);
    notifyListeners();
  }

  void toggleInspectorVisible() {
    setInspectorVisible(!inspectorVisible);
  }

  void selectInspectorNode(String? nodeId) {
    if (inspectorNodeId == nodeId) return;
    inspectorNodeId = nodeId;
    notifyListeners();
  }

  Future<void> logout() async {
    await _run(() async {
      if (server != null && refreshToken.isNotEmpty) {
        try {
          await NativeBridge.callOnWorker('auth.logout', {
            'server': server!.toJson(),
            'access_token': accessToken,
            'refresh_token': refreshToken,
          });
        } catch (_) {
          // Logout should clear local state even when the remote token is already invalid.
        }
      }
      _deleteStoreValue('refresh_token');
      accessToken = '';
      refreshToken = '';
      session = null;
      selectedTaskId = null;
      navigationSource = NavigationSource.controller;
      currentSection = AppSection.overview;
      _notifyRouter();
    });
  }

  void _notifyRouter() {
    _routerNotifier.notifyListeners();
  }

  Future<void> _run(Future<void> Function() action) async {
    busy = true;
    errorMessage = null;
    notifyListeners();
    try {
      await action();
    } catch (error) {
      errorMessage = error.toString();
      rethrow;
    } finally {
      busy = false;
      notifyListeners();
    }
  }

  List<ServerProfile> _readServerProfiles() {
    final value = _localStore['server_profiles'];
    if (value == null) return const [];
    try {
      final decoded = value is String ? jsonDecode(value) : value;
      if (decoded is List) {
        return decoded
            .whereType<Map>()
            .map((item) => ServerProfile.fromJson(item.cast<String, Object?>()))
            .where((item) => item.baseUrl.isNotEmpty)
            .toList();
      }
    } catch (_) {
      return const [];
    }
    return const [];
  }

  void _upsertServerProfile(ServerProfile profile) {
    final next = serverProfiles.where((item) => item.id != profile.id).toList();
    next.insert(0, profile);
    serverProfiles = next.take(12).toList();
  }

  void _activateServerProfile(String baseUrl) {
    final normalizedBaseUrl = baseUrl.trim();
    server = ServerProfile(
      id: normalizedBaseUrl,
      name: normalizedBaseUrl,
      baseUrl: normalizedBaseUrl,
    );
    _upsertServerProfile(server!);
    _writeLocalStoreValue(
      'server_profiles',
      serverProfiles.map((item) => item.toJson()).toList(),
    );
    _writeLocalStoreValue('active_server_id', server!.id);
  }

  void _loadLocalStore() {
    final file = _localStorePath();
    _localStoreFile = file;
    if (!file.existsSync()) {
      _localStore = {};
      return;
    }
    try {
      final decoded = jsonDecode(file.readAsStringSync());
      _localStore = decoded is Map
          ? decoded.cast<String, Object?>()
          : <String, Object?>{};
    } catch (_) {
      _localStore = {};
    }
  }

  void _loadPreferences() {
    final storedTheme = _readLocalStoreString('theme_mode');
    themeMode = storedTheme == 'dark' ? ThemeMode.dark : ThemeMode.light;
    final refresh = _localStore['auto_refresh_seconds'];
    if (refresh is num) {
      autoRefreshSeconds = refresh.toInt();
    }
    final storedInspectorVisible = _localStore['inspector_visible'];
    if (storedInspectorVisible is bool) {
      inspectorVisible = storedInspectorVisible;
    }
  }

  void _migrateLegacyLocalStoreIfNeeded() {
    if (_localStore.containsKey('server_profiles')) return;

    String? legacyProfiles;
    try {
      legacyProfiles = _readStoreValue('server_profiles');
    } catch (_) {
      return;
    }
    if (legacyProfiles == null || legacyProfiles.isEmpty) return;

    try {
      final decoded = jsonDecode(legacyProfiles);
      if (decoded is! List) return;
      final profiles = decoded
          .whereType<Map>()
          .map((item) => ServerProfile.fromJson(item.cast<String, Object?>()))
          .where((item) => item.baseUrl.isNotEmpty)
          .toList();
      if (profiles.isEmpty) return;

      _writeLocalStoreValue(
          'server_profiles', profiles.map((item) => item.toJson()).toList());
      _writeLocalStoreValue('active_server_id', profiles.first.id);
    } catch (_) {
      return;
    }
  }

  String? _firstServerProfileId() {
    try {
      final value = _localStore['server_profiles'];
      final decoded = value is String ? jsonDecode(value) : value;
      if (decoded is! List) return null;
      for (final item in decoded.whereType<Map>()) {
        final profile = ServerProfile.fromJson(item.cast<String, Object?>());
        if (profile.id.isNotEmpty) return profile.id;
      }
    } catch (_) {
      return null;
    }
    return null;
  }

  String? _readLocalStoreString(String key) {
    final value = _localStore[key];
    return value is String ? value : null;
  }

  void _writeLocalStoreValue(String key, Object? value) {
    _localStore[key] = value;
    final file = _localStoreFile ?? _localStorePath();
    _localStoreFile = file;
    file.parent.createSync(recursive: true);
    file.writeAsStringSync(
        const JsonEncoder.withIndent('  ').convert(_localStore));
  }

  File _localStorePath() {
    return File('${_appSupportDir().path}${Platform.pathSeparator}state.json');
  }

  Directory _appSupportDir() {
    if (Platform.isMacOS) {
      return Directory(_joinPath([
        Platform.environment['HOME'],
        'Library',
        'Application Support',
        'StreamServerDesktop',
      ]));
    }
    if (Platform.isWindows) {
      return Directory(_joinPath([
        Platform.environment['APPDATA'],
        'StreamServerDesktop',
      ]));
    }
    final xdgConfig = Platform.environment['XDG_CONFIG_HOME'];
    if (xdgConfig != null && xdgConfig.isNotEmpty) {
      return Directory(_joinPath([xdgConfig, 'streamserver-desktop']));
    }
    return Directory(_joinPath([
      Platform.environment['HOME'],
      '.config',
      'streamserver-desktop',
    ]));
  }

  String _joinPath(List<String?> segments) {
    return segments
        .where((segment) => segment != null && segment.isNotEmpty)
        .cast<String>()
        .join(Platform.pathSeparator);
  }

  String? _readStoreValue(String key) {
    final response = _bridge.call('secure_store.read', {'key': key});
    return response['value'] as String?;
  }

  void _writeStoreValue(String key, String value) {
    _bridge.call('secure_store.write', {'key': key, 'value': value});
  }

  void _deleteStoreValue(String key) {
    _bridge.call('secure_store.delete', {'key': key});
  }

  void _deleteStoreValueIfPresent(String key) {
    try {
      _deleteStoreValue(key);
    } catch (_) {
      // Secure store cleanup should not block an explicit unauthenticated login.
    }
  }

  T? _firstOrNull<T>(Iterable<T> values) {
    final iterator = values.iterator;
    return iterator.moveNext() ? iterator.current : null;
  }
}

class AppScope extends InheritedNotifier<AppController> {
  const AppScope({
    required AppController controller,
    required super.child,
    super.key,
  }) : super(notifier: controller);

  static AppController of(BuildContext context) {
    final scope = context.dependOnInheritedWidgetOfExactType<AppScope>();
    if (scope?.notifier == null) {
      throw StateError('AppScope was not found');
    }
    return scope!.notifier!;
  }
}
