import 'package:flutter/material.dart';
import 'package:go_router/go_router.dart';

import '../screens/artifacts_screen.dart';
import '../screens/login_screen.dart';
import '../screens/media_upload_screen.dart';
import '../screens/nodes_screen.dart';
import '../screens/overview_screen.dart';
import '../screens/records_screen.dart';
import '../screens/security_screen.dart';
import '../screens/streams_screen.dart';
import '../screens/task_create_screen.dart';
import '../screens/task_detail_screen.dart';
import '../screens/tasks_screen.dart';
import '../core/theme/stream_theme.dart';
import '../state.dart';
import '../widgets/app_shell.dart';

GoRouter buildAppRouter(AppController controller) {
  return GoRouter(
    initialLocation: sectionPath(controller.currentSection),
    refreshListenable: controller.routerListenable,
    redirect: (context, state) {
      final path = state.uri.path;
      if (!controller.initialized) {
        return path == '/loading' ? null : '/loading';
      }
      if (!controller.isAuthenticated) {
        return path == '/login' ? null : '/login';
      }
      if (path == '/login' || path == '/loading' || path == '/') {
        return sectionPath(controller.currentSection);
      }
      final currentPath = sectionPath(controller.currentSection);
      if (path != currentPath &&
          controller.navigationSource == NavigationSource.controller) {
        return currentPath;
      }
      return null;
    },
    routes: [
      GoRoute(
        path: '/loading',
        pageBuilder: (context, state) => NoTransitionPage(
          key: state.pageKey,
          child: Scaffold(
            backgroundColor: context.streamColors.appBackground,
            body: Center(
              child: Column(
                mainAxisSize: MainAxisSize.min,
                children: [
                  CircularProgressIndicator(
                      color: context.streamColors.primary),
                  const SizedBox(height: 14),
                  Text(
                    '正在载入 StreamServer 控制台',
                    style: TextStyle(color: context.streamColors.textSecondary),
                  ),
                ],
              ),
            ),
          ),
        ),
      ),
      GoRoute(
        path: '/login',
        pageBuilder: (context, state) => const NoTransitionPage(
          child: LoginScreen(),
        ),
      ),
      for (final entry in _routes.entries)
        GoRoute(
          path: entry.value,
          pageBuilder: (context, state) {
            controller.syncSectionFromRoute(entry.key);
            return NoTransitionPage(
              key: state.pageKey,
              child: AppShell(
                current: entry.key,
                onNavigate: (section) => context.go(sectionPath(section)),
                child: screenFor(entry.key),
              ),
            );
          },
        ),
    ],
  );
}

Widget screenFor(AppSection section) {
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
  };
}

String sectionPath(AppSection section) => _routes[section] ?? '/overview';

const _routes = {
  AppSection.overview: '/overview',
  AppSection.tasks: '/tasks',
  AppSection.taskCreate: '/tasks/create',
  AppSection.taskDetail: '/tasks/detail',
  AppSection.streams: '/streams',
  AppSection.multicast: '/multicast',
  AppSection.records: '/records',
  AppSection.artifacts: '/artifacts',
  AppSection.uploads: '/uploads',
  AppSection.nodes: '/nodes',
  AppSection.security: '/security',
};
