import 'package:flutter/material.dart';
import 'package:lucide_icons_flutter/lucide_icons.dart';

import '../core/theme/stream_theme.dart';
import '../core/widgets/stream_data_grid.dart';
import '../state.dart';
import '../utils.dart';
import '../widgets/app_select_field.dart';
import '../widgets/data_panel.dart';
import 'screen_helpers.dart';

class TasksScreen extends StatefulWidget {
  const TasksScreen({super.key});

  @override
  State<TasksScreen> createState() => _TasksScreenState();
}

class _TasksScreenState extends State<TasksScreen> {
  final keywordController = TextEditingController();
  final nodeController = TextEditingController();
  String statusFilter = '';
  String typeFilter = '';
  int page = 1;
  final int pageSize = 50;
  int refreshSeed = 0;
  Set<String> selectedTaskIds = {};
  String? pendingBatchAction;

  @override
  void dispose() {
    keywordController.dispose();
    nodeController.dispose();
    super.dispose();
  }

  void _refresh({bool resetPage = false}) {
    setState(() {
      if (resetPage) page = 1;
      refreshSeed++;
    });
  }

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        PageHeader(
          title: '任务中心',
          description: '查看任务状态，执行启动、停止、取消、重试、克隆和删除等控制操作。',
          actions: FilledButton.icon(
            onPressed: () => controller.navigate(AppSection.taskCreate),
            icon: const Icon(Icons.add),
            label: const Text('新建任务'),
          ),
        ),
        Surface(
          child: FilterBar(
            onApply: () => _refresh(resetPage: true),
            onReset: () {
              keywordController.clear();
              nodeController.clear();
              statusFilter = '';
              typeFilter = '';
              _refresh(resetPage: true);
            },
            children: [
              SmallTextField(
                controller: keywordController,
                label: '关键字',
                width: 260,
                onSubmitted: (_) => _refresh(resetPage: true),
              ),
              SmallSelect(
                label: '状态',
                value: statusFilter,
                options: const [
                  '',
                  'CREATED',
                  'VALIDATING',
                  'QUEUED',
                  'STARTING',
                  'RUNNING',
                  'STOPPING',
                  'RECOVERING',
                  'SUCCEEDED',
                  'FAILED',
                  'LOST',
                  'CANCELED',
                ],
                onChanged: (value) => setState(() => statusFilter = value),
              ),
              SmallSelect(
                label: '类型',
                value: typeFilter,
                options: const [
                  '',
                  'stream_ingest',
                  'stream_bridge',
                  'file_transcode'
                ],
                onChanged: (value) => setState(() => typeFilter = value),
                width: 210,
              ),
              SmallTextField(
                controller: nodeController,
                label: '节点 ID',
                width: 220,
                onSubmitted: (_) => _refresh(resetPage: true),
              ),
            ],
          ),
        ),
        const SizedBox(height: 12),
        AsyncDataPanel(
          key: ValueKey(refreshSeed),
          loader: (controller) => controller.api(
            'GET',
            '/api/v1/tasks',
            query: cleanQuery({
              'page': page,
              'page_size': pageSize,
              'keyword': keywordController.text,
              'status': statusFilter,
              'type': typeFilter,
              'node_id': nodeController.text,
              'sort_by': 'created_at',
              'sort_order': 'desc',
            }),
          ),
          builder: (context, data) {
            final payload = (data as Map).cast<String, Object?>();
            final rows = rowsFrom(payload['items']);
            final total = (payload['total'] as num?)?.toInt() ?? rows.length;
            final selectedRows = rows
                .where((row) => selectedTaskIds.contains('${row['id']}'))
                .toList();
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Align(
                  alignment: Alignment.centerRight,
                  child: PagerBar(
                    page: page,
                    pageSize: pageSize,
                    total: total,
                    onPageChanged: (value) {
                      page = value;
                      _refresh();
                    },
                  ),
                ),
                Surface(
                  child: Column(
                    crossAxisAlignment: CrossAxisAlignment.start,
                    children: [
                      if (selectedRows.isNotEmpty)
                        _TaskBatchActionsBar(
                          selectedRows: selectedRows,
                          pendingAction: pendingBatchAction,
                          onAction: (action) =>
                              _runBatchAction(context, action, selectedRows),
                        ),
                      LayoutBuilder(
                        builder: (context, constraints) {
                          if (constraints.maxWidth < 820) {
                            return _CompactTaskList(
                              rows: rows,
                              onDone: _refresh,
                            );
                          }
                          return StreamDataGrid(
                            height: 560,
                            rowHeight: 64,
                            rows: rows,
                            checkedRowKeys:
                                selectedTaskIds.map<Object>((id) => id).toSet(),
                            rowKey: (row) => '${row['id']}',
                            onCheckedRowsChanged: (checkedRows) {
                              setState(() {
                                selectedTaskIds = checkedRows
                                    .map((row) => '${row['id']}')
                                    .toSet();
                              });
                            },
                            onSelected: (row) =>
                                controller.openTask('${row['id']}'),
                            onDoubleTap: (row) =>
                                controller.openTask('${row['id']}'),
                            onSecondaryTap: (row) =>
                                _showTaskContextMenu(context, row, _refresh),
                            columns: [
                              StreamGridColumn(
                                title: '任务',
                                field: 'name',
                                width: 260,
                                enableRowChecked: true,
                                renderer: (context, row, value) => gridTextCell(
                                  context,
                                  value,
                                  fontWeight: FontWeight.w800,
                                  maxWidth: 248,
                                ),
                              ),
                              StreamGridColumn(
                                title: '类型',
                                field: 'type',
                                width: 150,
                                renderer: (context, row, value) =>
                                    gridTextCell(context, value, maxWidth: 140),
                              ),
                              StreamGridColumn(
                                title: '状态',
                                field: 'status',
                                width: 130,
                                renderer: (context, row, value) =>
                                    gridStatusCell(context, value),
                              ),
                              StreamGridColumn(
                                title: '节点',
                                field: 'assigned_node_id',
                                width: 110,
                                renderer: (context, row, value) => Text(shortId(
                                    row['assigned_node_id'] ?? row['node_id'])),
                              ),
                              const StreamGridColumn(
                                title: '创建者',
                                field: 'created_by',
                                width: 120,
                              ),
                              const StreamGridColumn(
                                title: '更新时间',
                                field: 'updated_at',
                                width: 210,
                              ),
                              StreamGridColumn(
                                title: '操作',
                                field: 'id',
                                width: 360,
                                minWidth: 320,
                                renderer: (context, row, value) =>
                                    _TaskRowActions(
                                  row: row,
                                  onDone: _refresh,
                                ),
                              ),
                            ],
                          );
                        },
                      ),
                    ],
                  ),
                ),
              ],
            );
          },
        ),
      ],
    );
  }

  Future<void> _runBatchAction(
    BuildContext context,
    String action,
    List<Map<String, Object?>> selectedRows,
  ) async {
    if (pendingBatchAction != null) return;
    final controller = AppScope.of(context);
    final config = _taskOperationConfig(action);
    final supportedRows = _tasksSupportingOperation(selectedRows, action);
    final skippedCount = selectedRows.length - supportedRows.length;
    if (supportedRows.isEmpty) {
      showResult(
        context,
        '已选任务都不支持批量${config.label}',
        tone: InlineStatusTone.info,
      );
      return;
    }

    final skipText = skippedCount > 0 ? '，$skippedCount 个不支持该操作的任务会跳过' : '';
    final message = config.destructive
        ? '确认删除 ${supportedRows.length} 个任务吗$skipText？该操作会同时删除其尝试记录、事件、录像与产物索引。'
        : '确认对 ${supportedRows.length} 个任务执行${config.label}吗$skipText？';
    final confirmed = await confirmAction(
      context,
      title: '批量任务操作',
      message: message,
      confirmLabel: '批量${config.label}',
      destructive: config.destructive,
    );
    if (!confirmed || !context.mounted) return;

    setState(() => pendingBatchAction = action);
    final failures = <Object>[];
    var successCount = 0;
    try {
      final results = await Future.wait(
        supportedRows.map((row) async {
          try {
            await _executeTaskOperation(controller, row, action);
            return null;
          } catch (cause) {
            return cause;
          }
        }),
      );
      for (final result in results) {
        if (result == null) {
          successCount++;
        } else {
          failures.add(result);
        }
      }
    } finally {
      if (mounted) {
        setState(() {
          pendingBatchAction = null;
          if (successCount > 0) selectedTaskIds.clear();
        });
      }
    }
    if (!context.mounted) return;

    if (successCount > 0) {
      final successText = config.destructive
          ? '已删除 $successCount 个任务'
          : '已提交 $successCount 个任务的${config.label}请求';
      showResult(
        context,
        skippedCount > 0 ? '$successText，部分任务已跳过' : successText,
        tone: InlineStatusTone.success,
      );
    } else if (skippedCount > 0) {
      showResult(context, '部分任务不支持该操作，已跳过');
    }

    if (failures.isNotEmpty) {
      showResult(
        context,
        '${failures.length} 个任务操作失败：${failures.first}',
        tone: InlineStatusTone.danger,
      );
    }
    _refresh();
  }
}

Future<void> _showTaskContextMenu(
  BuildContext context,
  Map<String, Object?> row,
  VoidCallback onDone,
) async {
  final regularActions =
      _rowOperationConfigs(row).where((config) => !config.destructive);
  final deleteAction = _firstOrNull(
    _rowOperationConfigs(row).where((config) => config.destructive),
  );
  final overlay = Overlay.of(context).context.findRenderObject() as RenderBox?;
  final size = overlay?.size ?? const Size(1, 1);
  final selected = await showMenu<String>(
    context: context,
    position: RelativeRect.fromLTRB(size.width / 2, size.height / 2, 24, 24),
    items: [
      PopupMenuItem<String>(
        enabled: false,
        height: 0,
        padding: EdgeInsets.zero,
        child: Builder(
          builder: (menuContext) {
            void choose(String value) => Navigator.of(menuContext).pop(value);
            return Column(
              mainAxisSize: MainAxisSize.min,
              children: [
                StreamMenuOption(
                  width: 156,
                  label: '打开详情',
                  icon: LucideIcons.eye,
                  onPressed: () => choose('detail'),
                ),
                const StreamMenuDivider(width: 156),
                for (final action in regularActions)
                  StreamMenuOption(
                    width: 156,
                    label: action.label,
                    icon: action.icon,
                    onPressed: () => choose(action.key),
                  ),
                if (_canCloneTask(row))
                  StreamMenuOption(
                    width: 156,
                    label: '克隆',
                    icon: LucideIcons.copy,
                    onPressed: () => choose('clone'),
                  ),
                if (deleteAction != null) ...[
                  const StreamMenuDivider(width: 156),
                  StreamMenuOption(
                    width: 156,
                    label: deleteAction.label,
                    icon: deleteAction.icon,
                    destructive: true,
                    onPressed: () => choose(deleteAction.key),
                  ),
                ],
              ],
            );
          },
        ),
      ),
    ],
  );
  if (selected == null || !context.mounted) return;
  if (selected == 'detail') {
    AppScope.of(context).openTask('${row['id']}');
  } else if (selected == 'clone') {
    await _cloneTask(context, row, onDone);
  } else {
    await _runTaskOperation(context, row, selected, onDone);
  }
}

class _CompactTaskList extends StatelessWidget {
  const _CompactTaskList({required this.rows, required this.onDone});

  final List<Map<String, Object?>> rows;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    if (rows.isEmpty) {
      return const SizedBox(
        height: 120,
        child: Center(child: Text('暂无任务')),
      );
    }
    return Column(
      children: [
        for (var index = 0; index < rows.length; index++) ...[
          _CompactTaskItem(row: rows[index], onDone: onDone),
          if (index != rows.length - 1)
            const Divider(height: 24, color: Color(0xffe4e8f0)),
        ],
      ],
    );
  }
}

class _CompactTaskItem extends StatelessWidget {
  const _CompactTaskItem({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final controller = AppScope.of(context);
    final id = '${row['id']}';
    return InkWell(
      borderRadius: BorderRadius.circular(8),
      onTap: () => controller.openTask(id),
      child: Padding(
        padding: const EdgeInsets.symmetric(vertical: 4),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Expanded(
                  child: Text(
                    textValue(row['name']),
                    softWrap: true,
                    style: const TextStyle(fontWeight: FontWeight.w700),
                  ),
                ),
                const SizedBox(width: 10),
                StatusBadge(status: row['status']),
              ],
            ),
            const SizedBox(height: 10),
            Wrap(
              spacing: 14,
              runSpacing: 8,
              children: [
                _TaskMeta(label: '类型', value: row['type']),
                _TaskMeta(
                  label: '节点',
                  value: shortId(row['assigned_node_id'] ?? row['node_id']),
                ),
                _TaskMeta(label: '创建者', value: row['created_by']),
                _TaskMeta(label: '更新时间', value: row['updated_at']),
              ],
            ),
            const SizedBox(height: 8),
            _TaskRowActions(row: row, onDone: onDone),
          ],
        ),
      ),
    );
  }
}

class _TaskMeta extends StatelessWidget {
  const _TaskMeta({required this.label, required this.value});

  final String label;
  final Object? value;

  @override
  Widget build(BuildContext context) {
    return ConstrainedBox(
      constraints: const BoxConstraints(maxWidth: 260),
      child: RichText(
        text: metadataTextSpan(context, label: label, value: value),
        softWrap: true,
      ),
    );
  }
}

class _TaskBatchActionsBar extends StatelessWidget {
  const _TaskBatchActionsBar({
    required this.selectedRows,
    required this.pendingAction,
    required this.onAction,
  });

  final List<Map<String, Object?>> selectedRows;
  final String? pendingAction;
  final ValueChanged<String> onAction;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final operations = _availableTaskOperations(selectedRows);
    return Padding(
      padding: const EdgeInsets.only(bottom: 12),
      child: LayoutBuilder(
        builder: (context, constraints) {
          final summary = Text(
            '已选 ${selectedRows.length} 个任务',
            style: TextStyle(
              color: colors.textSecondary,
              fontSize: 13,
              fontWeight: FontWeight.w700,
            ),
          );
          final actions = operations.isEmpty
              ? Text(
                  '当前选择没有可执行的批量操作',
                  style: TextStyle(color: colors.textSecondary, fontSize: 13),
                )
              : Wrap(
                  spacing: 8,
                  runSpacing: 8,
                  alignment: WrapAlignment.end,
                  children: [
                    for (final operation in operations)
                      _BatchActionButton(
                        operation: operation,
                        busy: pendingAction == operation.key,
                        disabled: pendingAction != null,
                        onPressed: () => onAction(operation.key),
                      ),
                  ],
                );
          if (constraints.maxWidth < 760) {
            return Column(
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                summary,
                const SizedBox(height: 8),
                actions,
              ],
            );
          }
          return Row(
            children: [
              Expanded(child: summary),
              Flexible(
                child: Align(
                  alignment: Alignment.centerRight,
                  child: actions,
                ),
              ),
            ],
          );
        },
      ),
    );
  }
}

class _BatchActionButton extends StatelessWidget {
  const _BatchActionButton({
    required this.operation,
    required this.busy,
    required this.disabled,
    required this.onPressed,
  });

  final _TaskOperationConfig operation;
  final bool busy;
  final bool disabled;
  final VoidCallback onPressed;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    final icon = busy
        ? SizedBox(
            width: 14,
            height: 14,
            child: CircularProgressIndicator(
              strokeWidth: 2,
              color: operation.destructive ? colors.danger : colors.primary,
            ),
          )
        : Icon(operation.icon, size: 16);
    return OutlinedButton.icon(
      style: operation.destructive
          ? OutlinedButton.styleFrom(
              foregroundColor: colors.danger,
              side: BorderSide(color: colors.danger.withValues(alpha: 0.55)),
            )
          : null,
      onPressed: disabled ? null : onPressed,
      icon: icon,
      label: Text('批量${operation.label}'),
    );
  }
}

class _TaskRowActions extends StatelessWidget {
  const _TaskRowActions({required this.row, required this.onDone});

  final Map<String, Object?> row;
  final VoidCallback onDone;

  @override
  Widget build(BuildContext context) {
    final operations = _rowOperationConfigs(row);
    final regularOperations =
        operations.where((operation) => !operation.destructive);
    final deleteOperation =
        _firstOrNull(operations.where((operation) => operation.destructive));
    return Wrap(
      spacing: 2,
      runSpacing: 2,
      crossAxisAlignment: WrapCrossAlignment.center,
      children: [
        _TaskActionButton(
          label: '详情',
          icon: LucideIcons.eye,
          onPressed: () => AppScope.of(context).openTask('${row['id']}'),
        ),
        for (final operation in regularOperations)
          _TaskActionButton(
            label: operation.label,
            icon: operation.icon,
            onPressed: () =>
                _runTaskOperation(context, row, operation.key, onDone),
          ),
        if (_canCloneTask(row))
          _TaskActionButton(
            label: '克隆',
            icon: LucideIcons.copy,
            onPressed: () => _cloneTask(context, row, onDone),
          ),
        if (deleteOperation != null)
          _TaskActionButton(
            label: deleteOperation.label,
            icon: deleteOperation.icon,
            destructive: true,
            onPressed: () =>
                _runTaskOperation(context, row, deleteOperation.key, onDone),
          ),
      ],
    );
  }
}

class _TaskActionButton extends StatelessWidget {
  const _TaskActionButton({
    required this.label,
    required this.icon,
    required this.onPressed,
    this.destructive = false,
  });

  final String label;
  final IconData icon;
  final VoidCallback onPressed;
  final bool destructive;

  @override
  Widget build(BuildContext context) {
    final colors = context.streamColors;
    return TextButton.icon(
      style: TextButton.styleFrom(
        minimumSize: const Size(0, 30),
        padding: const EdgeInsets.symmetric(horizontal: 7, vertical: 4),
        tapTargetSize: MaterialTapTargetSize.shrinkWrap,
        foregroundColor: destructive ? colors.danger : colors.primary,
      ),
      onPressed: onPressed,
      icon: Icon(icon, size: 15),
      label: Text(label),
    );
  }
}

class _TaskOperationConfig {
  const _TaskOperationConfig({
    required this.key,
    required this.label,
    required this.supportedStatuses,
    required this.icon,
    this.destructive = false,
  });

  final String key;
  final String label;
  final List<String> supportedStatuses;
  final IconData icon;
  final bool destructive;
}

const _taskOperationConfigs = [
  _TaskOperationConfig(
    key: 'start',
    label: '启动',
    supportedStatuses: ['CREATED', 'VALIDATING', 'FAILED', 'CANCELED'],
    icon: LucideIcons.play,
  ),
  _TaskOperationConfig(
    key: 'stop',
    label: '停止',
    supportedStatuses: [
      'DISPATCHING',
      'STARTING',
      'RUNNING',
      'RECOVERING',
      'LOST'
    ],
    icon: LucideIcons.square,
  ),
  _TaskOperationConfig(
    key: 'cancel',
    label: '取消',
    supportedStatuses: [
      'CREATED',
      'VALIDATING',
      'QUEUED',
      'DISPATCHING',
      'STARTING',
      'RUNNING',
      'RECOVERING',
    ],
    icon: LucideIcons.circleX,
  ),
  _TaskOperationConfig(
    key: 'retry',
    label: '重试',
    supportedStatuses: ['FAILED', 'LOST'],
    icon: LucideIcons.rotateCw,
  ),
  _TaskOperationConfig(
    key: 'delete',
    label: '删除',
    supportedStatuses: [
      'CREATED',
      'VALIDATING',
      'QUEUED',
      'SUCCEEDED',
      'FAILED',
      'CANCELED',
      'LOST',
    ],
    icon: LucideIcons.trash2,
    destructive: true,
  ),
];

const _cloneableStatuses = ['SUCCEEDED', 'FAILED', 'CANCELED', 'LOST'];

_TaskOperationConfig _taskOperationConfig(String action) {
  return _taskOperationConfigs.firstWhere((config) => config.key == action);
}

bool _canRunTaskOperation(Map<String, Object?> task, String action) {
  final status = _taskStatus(task);
  return _taskOperationConfig(action).supportedStatuses.contains(status);
}

bool _canCloneTask(Map<String, Object?> task) {
  return _cloneableStatuses.contains(_taskStatus(task));
}

List<_TaskOperationConfig> _rowOperationConfigs(Map<String, Object?> task) {
  return _taskOperationConfigs
      .where((config) => _canRunTaskOperation(task, config.key))
      .toList();
}

List<_TaskOperationConfig> _availableTaskOperations(
  List<Map<String, Object?>> tasks,
) {
  return _taskOperationConfigs
      .where((config) =>
          tasks.any((task) => _canRunTaskOperation(task, config.key)))
      .toList();
}

List<Map<String, Object?>> _tasksSupportingOperation(
  List<Map<String, Object?>> tasks,
  String action,
) {
  return tasks.where((task) => _canRunTaskOperation(task, action)).toList();
}

String _taskStatus(Map<String, Object?> task) {
  return textValue(task['status']).trim().toUpperCase();
}

Future<void> _executeTaskOperation(
  AppController controller,
  Map<String, Object?> task,
  String action,
) async {
  final id = '${task['id']}';
  final path = '/api/v1/tasks/$id';
  switch (action) {
    case 'start':
      await controller.api('POST', '$path/start');
      return;
    case 'stop':
      await controller.api('POST', '$path/stop');
      return;
    case 'cancel':
      await controller.api('POST', '$path/cancel');
      return;
    case 'retry':
      await controller.api('POST', '$path/retry');
      return;
    case 'delete':
      await controller.api('DELETE', path);
      return;
  }
}

Future<void> _runTaskOperation(
  BuildContext context,
  Map<String, Object?> task,
  String action,
  VoidCallback onDone,
) async {
  final controller = AppScope.of(context);
  final config = _taskOperationConfig(action);
  final name = textValue(task['name']);
  final message = config.destructive
      ? '确认删除任务 $name 吗？该操作会同时删除其尝试记录、事件、录像与产物索引。'
      : '确认对任务 $name 执行${config.label}吗？';
  final confirmed = await confirmAction(
    context,
    title: '任务操作',
    message: message,
    confirmLabel: config.label,
    destructive: config.destructive,
  );
  if (!confirmed || !context.mounted) return;
  try {
    await _executeTaskOperation(controller, task, action);
    if (context.mounted) {
      showResult(
        context,
        config.destructive ? '任务已删除' : '已提交${config.label}请求',
        tone: InlineStatusTone.success,
      );
    }
    onDone();
  } catch (cause) {
    if (context.mounted) {
      showResult(context, cause.toString(), tone: InlineStatusTone.danger);
    }
  }
}

Future<void> _cloneTask(
  BuildContext context,
  Map<String, Object?> task,
  VoidCallback onDone,
) async {
  final controller = AppScope.of(context);
  final id = '${task['id']}';
  try {
    final cloned = await controller.api(
      'POST',
      '/api/v1/tasks/$id/clone',
      body: {},
    );
    if (!context.mounted) return;
    final clonedId = _clonedTaskId(cloned);
    showResult(
      context,
      clonedId == null ? '任务已克隆' : '已克隆任务 ${shortId(clonedId)}',
      tone: InlineStatusTone.success,
    );
    if (clonedId == null) {
      onDone();
    } else {
      controller.openTask(clonedId);
    }
  } catch (cause) {
    if (context.mounted) {
      showResult(context, cause.toString(), tone: InlineStatusTone.danger);
    }
  }
}

String? _clonedTaskId(Map<String, Object?> payload) {
  final directId = payload['id'];
  if (directId != null) return '$directId';
  final task = payload['task'];
  if (task is Map && task['id'] != null) return '${task['id']}';
  return null;
}

T? _firstOrNull<T>(Iterable<T> items) {
  for (final item in items) {
    return item;
  }
  return null;
}
