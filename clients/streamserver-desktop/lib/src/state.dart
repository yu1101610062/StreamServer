import 'dart:convert';

import 'package:flutter/widgets.dart';

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
  debug,
}

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

  ServerProfile? server;
  List<ServerProfile> serverProfiles = const [];
  String accessToken = '';
  String refreshToken = '';
  Map<String, Object?>? session;
  AppSection currentSection = AppSection.overview;
  String? selectedTaskId;
  String? activeMediaUrl;
  String? activeMediaTitle;
  String? errorMessage;
  bool busy = false;
  bool initialized = false;

  bool get isAuthenticated => session != null;
  String get subject => (session?['subject'] as String?) ?? 'unknown';
  String get role => (session?['role'] as String?) ?? 'unknown';
  String get environment => (session?['environment'] as String?) ?? 'desktop';

  Future<void> initialize() async {
    if (initialized) return;
    busy = true;
    notifyListeners();
    try {
      serverProfiles = _readServerProfiles();
      final activeServerId = _readStoreValue('active_server_id');
      server = _firstOrNull(
          serverProfiles.where((profile) => profile.id == activeServerId));
      server ??= _firstOrNull(serverProfiles);
      refreshToken = _readStoreValue('refresh_token') ?? '';

      if (server != null && refreshToken.isNotEmpty) {
        try {
          final tokens = _bridge.call('auth.refresh', {
            'server': server!.toJson(),
            'refresh_token': refreshToken,
          });
          accessToken = (tokens['access_token'] as String?) ?? '';
          refreshToken = (tokens['refresh_token'] as String?) ?? refreshToken;
          _writeStoreValue('refresh_token', refreshToken);
          session = _bridge.call('auth.me', {
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
      notifyListeners();
    }
  }

  Future<void> login({
    required String baseUrl,
    required String username,
    required String password,
  }) async {
    await _run(() async {
      server = ServerProfile(id: baseUrl, name: baseUrl, baseUrl: baseUrl);
      _upsertServerProfile(server!);
      _writeStoreValue('server_profiles',
          jsonEncode(serverProfiles.map((item) => item.toJson()).toList()));
      _writeStoreValue('active_server_id', server!.id);
      final tokens = _bridge.call('auth.login', {
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
      session = _bridge.call('auth.me', {
        'server': server!.toJson(),
        'access_token': accessToken,
      });
      currentSection = AppSection.overview;
    });
  }

  void selectServer(ServerProfile profile) {
    server = profile;
    _writeStoreValue('active_server_id', profile.id);
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
    final response = _bridge.call('api.request', {
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
    if (nextRefreshToken != null && nextRefreshToken.isNotEmpty) {
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
    return _bridge.call('upload.media', {
      'server': active.toJson(),
      'access_token': accessToken,
      'file_path': filePath,
    });
  }

  void playMedia(String url, {String? title}) {
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
    return _bridge.call('media_player.open_external', {
      'body': {'url': url},
    });
  }

  Future<Map<String, Object?>> stopMedia(String sessionId) async {
    return _bridge.call('media_player.stop', {
      'body': {'session_id': sessionId},
    });
  }

  Future<Map<String, Object?>> snapshotMedia(String sessionId,
      {String? outputPath}) async {
    return _bridge.call('media_player.snapshot', {
      'body': {
        'session_id': sessionId,
        if (outputPath != null && outputPath.isNotEmpty)
          'output_path': outputPath,
      },
    });
  }

  Future<Map<String, Object?>> openMediaProbe() async {
    return _bridge.call('media_player.probe', {});
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
    return _bridge.call('diagnostics.probe', {
      'server': active.toJson(),
      'access_token': accessToken,
    });
  }

  void navigate(AppSection section) {
    currentSection = section;
    notifyListeners();
  }

  void openTask(String taskId) {
    selectedTaskId = taskId;
    currentSection = AppSection.taskDetail;
    notifyListeners();
  }

  Future<void> logout() async {
    await _run(() async {
      if (server != null && refreshToken.isNotEmpty) {
        try {
          _bridge.call('auth.logout', {
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
      currentSection = AppSection.overview;
    });
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
    final value = _readStoreValue('server_profiles');
    if (value == null || value.isEmpty) return const [];
    try {
      final decoded = jsonDecode(value);
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
