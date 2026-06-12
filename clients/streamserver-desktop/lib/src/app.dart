import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';

import 'app/providers.dart';
import 'app/router.dart';
import 'core/theme/stream_theme.dart';
import 'state.dart';

class StreamServerDesktopApp extends ConsumerStatefulWidget {
  const StreamServerDesktopApp({super.key});

  @override
  ConsumerState<StreamServerDesktopApp> createState() =>
      _StreamServerDesktopAppState();
}

class _StreamServerDesktopAppState
    extends ConsumerState<StreamServerDesktopApp> {
  AppController? _controller;
  RouterConfig<Object>? _router;

  @override
  Widget build(BuildContext context) {
    final controller = ref.watch(
      appControllerProvider.select((controller) => controller),
    );
    final themeMode = ref.watch(
      appControllerProvider.select((controller) => controller.themeMode),
    );
    if (!identical(controller, _controller)) {
      _controller = controller;
      _router = buildAppRouter(controller);
    }
    return AppScope(
      controller: controller,
      child: MaterialApp.router(
        title: 'StreamServer控制台',
        debugShowCheckedModeBanner: false,
        theme: StreamTheme.light(),
        darkTheme: StreamTheme.dark(),
        themeMode: themeMode,
        routerConfig: _router!,
      ),
    );
  }
}
