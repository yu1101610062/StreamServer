import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_riverpod/legacy.dart';

import '../bridge/native_bridge.dart';
import '../state.dart';

final appControllerProvider = ChangeNotifierProvider<AppController>((ref) {
  final controller = AppController(NativeBridge.instance);
  Future.microtask(controller.initialize);
  return controller;
});

final appRepositoryProvider = Provider<StreamRepository>((ref) {
  return StreamRepository(ref.watch(appControllerProvider));
});

class StreamRepository {
  const StreamRepository(this.controller);

  final AppController controller;

  Future<Map<String, Object?>> page(
    String path, {
    Map<String, Object?>? query,
  }) {
    return controller.api('GET', path, query: query);
  }

  Future<List<Map<String, Object?>>> list(
    String path, {
    Map<String, Object?>? query,
  }) {
    return controller.apiList(path, query: query);
  }

  Future<void> mutate(String method, String path, {Object? body}) {
    return controller.mutate(method, path, body: body);
  }
}
