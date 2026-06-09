import 'package:flutter/material.dart';

import 'bridge/native_bridge.dart';
import 'screens/artifacts_screen.dart';
import 'screens/debug_screen.dart';
import 'screens/login_screen.dart';
import 'screens/media_upload_screen.dart';
import 'screens/nodes_screen.dart';
import 'screens/overview_screen.dart';
import 'screens/records_screen.dart';
import 'screens/security_screen.dart';
import 'screens/streams_screen.dart';
import 'screens/task_create_screen.dart';
import 'screens/task_detail_screen.dart';
import 'screens/tasks_screen.dart';
import 'state.dart';
import 'widgets/app_shell.dart';

class StreamServerDesktopApp extends StatefulWidget {
  const StreamServerDesktopApp({super.key});

  @override
  State<StreamServerDesktopApp> createState() => _StreamServerDesktopAppState();
}

class _StreamServerDesktopAppState extends State<StreamServerDesktopApp> {
  late final AppController controller;

  @override
  void initState() {
    super.initState();
    controller = AppController(NativeBridge.instance);
    Future.microtask(controller.initialize);
  }

  @override
  Widget build(BuildContext context) {
    return AppScope(
      controller: controller,
      child: MaterialApp(
        title: 'StreamServer Desktop',
        debugShowCheckedModeBanner: false,
        theme: ThemeData(
          colorScheme: ColorScheme.fromSeed(
            seedColor: const Color(0xff1463ff),
            brightness: Brightness.light,
          ),
          scaffoldBackgroundColor: const Color(0xfff5f7fb),
          useMaterial3: true,
          cardTheme: const CardThemeData(
            shape: RoundedRectangleBorder(
              borderRadius: BorderRadius.all(Radius.circular(8)),
            ),
          ),
          dataTableTheme: const DataTableThemeData(
            headingRowColor: WidgetStatePropertyAll(Color(0xffeef3ff)),
          ),
        ),
        home: AnimatedBuilder(
          animation: controller,
          builder: (context, _) {
            if (!controller.initialized) {
              return const Scaffold(
                body: Center(child: CircularProgressIndicator()),
              );
            }
            if (!controller.isAuthenticated) {
              return const LoginScreen();
            }
            return AppShell(
              current: controller.currentSection,
              onNavigate: controller.navigate,
              child: _screenFor(controller.currentSection),
            );
          },
        ),
      ),
    );
  }

  Widget _screenFor(AppSection section) {
    return switch (section) {
      AppSection.overview => const OverviewScreen(),
      AppSection.tasks => const TasksScreen(),
      AppSection.taskCreate => const TaskCreateScreen(),
      AppSection.taskDetail => const TaskDetailScreen(),
      AppSection.streams => const StreamsScreen(key: ValueKey('streams')),
      AppSection.multicast =>
        const StreamsScreen(key: ValueKey('multicast'), schemaFilter: 'rtp'),
      AppSection.records => const RecordsScreen(),
      AppSection.artifacts => const ArtifactsScreen(),
      AppSection.uploads => const MediaUploadScreen(),
      AppSection.nodes => const NodesScreen(),
      AppSection.security => const SecurityScreen(),
      AppSection.debug => const DebugScreen(),
    };
  }
}
