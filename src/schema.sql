create table if not exists users (
	id integer primary key
);

create table if not exists games (
	id integer primary key,
	w_id integer,
	b_id integer,

	ended boolean,

	-- null - draw, 0 - black, 1 white
	winner boolean, 

	-- null - not over, 0 - timeout, 1 - resign, 2 - checkmate, 3 - draw
	termination integer, -- 

	fen text not null,

	foreign key (w_id) references users (id)
	foreign key (b_id) references users (id)
);

create table if not exists moves (
	game_id integer,
	ply integer not null,
	uci text not null,

	foreign key (game_id) references games (id)
);
