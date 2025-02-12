use super::utils::logitems::ItemBatch;
use super::visibility_blocking;
use crate::{
	components::{
		event_pump, CommandBlocking, CommandInfo, Component,
		DiffComponent, DrawableComponent, EventState, ScrollType,
	},
	keys::SharedKeyConfig,
	queue::{InternalEvent, NeedsUpdate, Queue},
	strings,
	ui::{draw_scrollbar, style::SharedTheme},
};
use anyhow::Result;
use asyncgit::{
	sync::{
		diff::DiffOptions, diff_contains_file, get_commits_info,
		CommitId, RepoPathRef,
	},
	AsyncDiff, AsyncGitNotification, AsyncLog, DiffParams, DiffType,
	FetchStatus,
};
use chrono::{DateTime, Local};
use crossbeam_channel::Sender;
use crossterm::event::Event;
use tui::{
	backend::Backend,
	layout::{Constraint, Direction, Layout, Rect},
	text::{Span, Spans, Text},
	widgets::{Block, Borders, Cell, Clear, Row, Table, TableState},
	Frame,
};

const SLICE_SIZE: usize = 1200;

///
pub struct FileRevlogComponent {
	git_log: Option<AsyncLog>,
	git_diff: AsyncDiff,
	theme: SharedTheme,
	queue: Queue,
	sender: Sender<AsyncGitNotification>,
	diff: DiffComponent,
	visible: bool,
	repo_path: RepoPathRef,
	file_path: Option<String>,
	table_state: std::cell::Cell<TableState>,
	items: ItemBatch,
	count_total: usize,
	key_config: SharedKeyConfig,
	current_width: std::cell::Cell<usize>,
	current_height: std::cell::Cell<usize>,
}

impl FileRevlogComponent {
	///
	pub fn new(
		repo_path: &RepoPathRef,
		queue: &Queue,
		sender: &Sender<AsyncGitNotification>,
		theme: SharedTheme,
		key_config: SharedKeyConfig,
	) -> Self {
		Self {
			theme: theme.clone(),
			queue: queue.clone(),
			sender: sender.clone(),
			diff: DiffComponent::new(
				repo_path.clone(),
				queue.clone(),
				theme,
				key_config.clone(),
				true,
			),
			git_log: None,
			git_diff: AsyncDiff::new(
				repo_path.borrow().clone(),
				sender,
			),
			visible: false,
			repo_path: repo_path.clone(),
			file_path: None,
			table_state: std::cell::Cell::new(TableState::default()),
			items: ItemBatch::default(),
			count_total: 0,
			key_config,
			current_width: std::cell::Cell::new(0),
			current_height: std::cell::Cell::new(0),
		}
	}

	fn components_mut(&mut self) -> Vec<&mut dyn Component> {
		vec![&mut self.diff]
	}

	///
	pub fn open(&mut self, file_path: &str) -> Result<()> {
		self.file_path = Some(file_path.into());

		let filter = diff_contains_file(
			self.repo_path.borrow().clone(),
			file_path.into(),
		);
		self.git_log = Some(AsyncLog::new(
			self.repo_path.borrow().clone(),
			&self.sender,
			Some(filter),
		));
		self.table_state.get_mut().select(Some(0));
		self.show()?;
		self.diff.clear(false);

		self.update()?;

		Ok(())
	}

	///
	pub fn any_work_pending(&self) -> bool {
		self.git_diff.is_pending()
			|| self
				.git_log
				.as_ref()
				.map_or(false, AsyncLog::is_pending)
	}

	///
	pub fn update(&mut self) -> Result<()> {
		if let Some(ref mut git_log) = self.git_log {
			let log_changed =
				git_log.fetch()? == FetchStatus::Started;

			let table_state = self.table_state.take();
			let start = table_state.selected().unwrap_or(0);
			self.table_state.set(table_state);

			if self.items.needs_data(start, git_log.count()?)
				|| log_changed
			{
				self.fetch_commits()?;
			}

			self.update_diff()?;
		}

		Ok(())
	}

	///
	pub fn update_git(
		&mut self,
		event: AsyncGitNotification,
	) -> Result<()> {
		if self.visible {
			match event {
				AsyncGitNotification::CommitFiles
				| AsyncGitNotification::Log => self.update()?,
				AsyncGitNotification::Diff => self.update_diff()?,
				_ => (),
			}
		}

		Ok(())
	}

	pub fn update_diff(&mut self) -> Result<()> {
		if self.is_visible() {
			if let Some(commit_id) = self.selected_commit() {
				if let Some(file_path) = &self.file_path {
					let diff_params = DiffParams {
						path: file_path.clone(),
						diff_type: DiffType::Commit(commit_id),
						options: DiffOptions::default(),
					};

					if let Some((params, last)) =
						self.git_diff.last()?
					{
						if params == diff_params {
							self.diff.update(
								file_path.to_string(),
								false,
								last,
							);

							return Ok(());
						}
					}

					self.git_diff.request(diff_params)?;
					self.diff.clear(true);

					return Ok(());
				}
			}

			self.diff.clear(false);
		}

		Ok(())
	}

	fn fetch_commits(&mut self) -> Result<()> {
		if let Some(git_log) = &mut self.git_log {
			let table_state = self.table_state.take();

			let start = table_state.selected().unwrap_or(0);

			let commits = get_commits_info(
				&self.repo_path.borrow(),
				&git_log.get_slice(start, SLICE_SIZE)?,
				self.current_width.get() as usize,
			);

			if let Ok(commits) = commits {
				self.items.set_items(start, commits);
			}

			self.table_state.set(table_state);
			self.count_total = git_log.count()?;
		}

		Ok(())
	}

	fn selected_commit(&self) -> Option<CommitId> {
		let table_state = self.table_state.take();

		let commit_id = table_state.selected().and_then(|selected| {
			self.items
				.iter()
				.nth(selected)
				.as_ref()
				.map(|entry| entry.id)
		});

		self.table_state.set(table_state);

		commit_id
	}

	fn can_focus_diff(&self) -> bool {
		self.selected_commit().is_some()
	}

	fn get_title(&self) -> String {
		self.file_path.as_ref().map_or(
			"<no history available>".into(),
			|file_path| {
				strings::file_log_title(&self.key_config, file_path)
			},
		)
	}

	fn get_rows(&self, now: DateTime<Local>) -> Vec<Row> {
		self.items
			.iter()
			.map(|entry| {
				let spans = Spans::from(vec![
					Span::styled(
						entry.hash_short.to_string(),
						self.theme.commit_hash(false),
					),
					Span::raw(" "),
					Span::styled(
						entry.time_to_string(now),
						self.theme.commit_time(false),
					),
					Span::raw(" "),
					Span::styled(
						entry.author.to_string(),
						self.theme.commit_author(false),
					),
				]);

				let mut text = Text::from(spans);
				text.extend(Text::raw(entry.msg.to_string()));

				let cells = vec![Cell::from(""), Cell::from(text)];

				Row::new(cells).height(2)
			})
			.collect()
	}

	fn get_max_selection(&mut self) -> usize {
		self.git_log.as_mut().map_or(0, |log| {
			log.count().unwrap_or(0).saturating_sub(1)
		})
	}

	fn move_selection(&mut self, scroll_type: ScrollType) -> bool {
		let mut table_state = self.table_state.take();

		let old_selection = table_state.selected().unwrap_or(0);
		let max_selection = self.get_max_selection();

		let new_selection = match scroll_type {
			ScrollType::Up => old_selection.saturating_sub(1),
			ScrollType::Down => {
				old_selection.saturating_add(1).min(max_selection)
			}
			ScrollType::Home => 0,
			ScrollType::End => max_selection,
			ScrollType::PageUp => old_selection.saturating_sub(
				self.current_height.get().saturating_sub(2),
			),
			ScrollType::PageDown => old_selection
				.saturating_add(
					self.current_height.get().saturating_sub(2),
				)
				.min(max_selection),
		};

		let needs_update = new_selection != old_selection;

		if needs_update {
			self.queue.push(InternalEvent::Update(NeedsUpdate::DIFF));
		}

		table_state.select(Some(new_selection));
		self.table_state.set(table_state);

		needs_update
	}

	fn draw_revlog<B: Backend>(&self, f: &mut Frame<B>, area: Rect) {
		let constraints = [
			// type of change: (A)dded, (M)odified, (D)eleted
			Constraint::Length(1),
			// commit details
			Constraint::Percentage(100),
		];

		let now = Local::now();

		let title = self.get_title();
		let rows = self.get_rows(now);

		let table = Table::new(rows)
			.widths(&constraints)
			.column_spacing(1)
			.highlight_style(self.theme.text(true, true))
			.block(
				Block::default()
					.borders(Borders::ALL)
					.title(Span::styled(
						title,
						self.theme.title(true),
					))
					.border_style(self.theme.block(true)),
			);

		let mut table_state = self.table_state.take();

		f.render_widget(Clear, area);
		f.render_stateful_widget(table, area, &mut table_state);

		draw_scrollbar(
			f,
			area,
			&self.theme,
			self.count_total,
			table_state.selected().unwrap_or(0),
		);

		self.table_state.set(table_state);
		self.current_width.set(area.width.into());
		self.current_height.set(area.height.into());
	}
}

impl DrawableComponent for FileRevlogComponent {
	fn draw<B: Backend>(
		&self,
		f: &mut Frame<B>,
		area: Rect,
	) -> Result<()> {
		if self.visible {
			let percentages = if self.diff.focused() {
				(30, 70)
			} else {
				(50, 50)
			};

			let chunks = Layout::default()
				.direction(Direction::Horizontal)
				.constraints(
					[
						Constraint::Percentage(percentages.0),
						Constraint::Percentage(percentages.1),
					]
					.as_ref(),
				)
				.split(area);

			f.render_widget(Clear, area);

			self.draw_revlog(f, chunks[0]);
			self.diff.draw(f, chunks[1])?;
		}

		Ok(())
	}
}

impl Component for FileRevlogComponent {
	fn event(&mut self, event: Event) -> Result<EventState> {
		if self.is_visible() {
			if event_pump(
				event,
				self.components_mut().as_mut_slice(),
			)?
			.is_consumed()
			{
				return Ok(EventState::Consumed);
			}

			if let Event::Key(key) = event {
				if key == self.key_config.keys.exit_popup {
					self.hide();

					return Ok(EventState::Consumed);
				} else if key == self.key_config.keys.focus_right
					&& self.can_focus_diff()
				{
					self.diff.focus(true);
					return Ok(EventState::Consumed);
				} else if key == self.key_config.keys.focus_left {
					if self.diff.focused() {
						self.diff.focus(false);
					} else {
						self.hide();
					}
					return Ok(EventState::Consumed);
				} else if key == self.key_config.keys.enter {
					self.hide();

					return self.selected_commit().map_or(
						Ok(EventState::NotConsumed),
						|id| {
							self.queue.push(
								InternalEvent::InspectCommit(
									id, None,
								),
							);
							Ok(EventState::Consumed)
						},
					);
				} else if key == self.key_config.keys.move_up {
					self.move_selection(ScrollType::Up)
				} else if key == self.key_config.keys.move_down {
					self.move_selection(ScrollType::Down)
				} else if key == self.key_config.keys.shift_up
					|| key == self.key_config.keys.home
				{
					self.move_selection(ScrollType::Home)
				} else if key == self.key_config.keys.shift_down
					|| key == self.key_config.keys.end
				{
					self.move_selection(ScrollType::End)
				} else if key == self.key_config.keys.page_up {
					self.move_selection(ScrollType::PageUp)
				} else if key == self.key_config.keys.page_down {
					self.move_selection(ScrollType::PageDown)
				} else {
					false
				};
			}

			return Ok(EventState::Consumed);
		}

		Ok(EventState::NotConsumed)
	}

	fn commands(
		&self,
		out: &mut Vec<CommandInfo>,
		force_all: bool,
	) -> CommandBlocking {
		if self.is_visible() || force_all {
			out.push(
				CommandInfo::new(
					strings::commands::close_popup(&self.key_config),
					true,
					true,
				)
				.order(1),
			);
			out.push(
				CommandInfo::new(
					strings::commands::log_details_toggle(
						&self.key_config,
					),
					true,
					self.selected_commit().is_some(),
				)
				.order(1),
			);

			out.push(CommandInfo::new(
				strings::commands::diff_focus_right(&self.key_config),
				self.can_focus_diff(),
				!self.diff.focused(),
			));
			out.push(CommandInfo::new(
				strings::commands::diff_focus_left(&self.key_config),
				true,
				self.diff.focused(),
			));
		}

		visibility_blocking(self)
	}

	fn is_visible(&self) -> bool {
		self.visible
	}

	fn hide(&mut self) {
		self.visible = false;
	}

	fn show(&mut self) -> Result<()> {
		self.visible = true;

		Ok(())
	}
}
